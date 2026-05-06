# Adversary Gate-1 Findings — ADR-042 transport-binding rewrite (2026-05-06)

**Type**: Adversary → Architect (rewrite review, pre-implementation)
**Date**: 2026-05-06
**Reviewer**: adversary (architecture mode)
**Mode**: review of the 2026-05-06 rewrite that separated the native gateway service contract from its transport bindings. The 2026-05-05 gate-1 + gate-1 round-2 findings on the contract layer remain applicable and are not re-reviewed here. This pass focuses on what the rewrite *introduced*: the contract/binding split, runtime probe + selection, per-binding auth models, dlopen behavior, heterogeneous deployments, and binding-related failure surfaces.
**Verdict**: **PASS WITH CONDITIONS — 0 CRITICAL, 4 HIGH, 6 MEDIUM, 3 LOW.** No structural redesign needed; the contract/binding architecture holds. The HIGH findings are spec gaps the architect must close before implementer work on bindings beyond gRPC begins.

## Summary

| Severity | Count |
|---|---|
| Critical | 0 |
| High | 4 |
| Medium | 6 |
| Low | 3 |

The rewrite is structurally sound. The contract/binding split is genuinely orthogonal — the contract types and invariants (I-NG1..I-NG14) carry over without modification. Most issues cluster around two surfaces: (1) the *security model heterogeneity* across bindings (gRPC and TCP-framed share mTLS-via-rustls; ibverbs and libfabric have provider-dependent trust anchors that the ADR doesn't fully pin down), and (2) the *runtime selection* logic at the edges (probe failures during startup, mid-flight binding crashes, heterogeneous-cluster client behavior under partial topology refreshes).

---

## CRITICAL

(none)

---

## HIGH

### H1: RDMA bindings' "trust anchor" is hand-waved; per-provider auth model isn't pinned

**Severity**: High
**Category**: Security > Trust boundaries
**Location**: ADR-042 §2.3 (ibverbs auth), §2.4 (libfabric auth), §5.1 (per-binding hook table)

**Description**: §2.3 says ibverbs auth is "mTLS-over-RDMA via rdma-cm + rxm provider, OR (where the operator's compliance posture permits) implicit auth via RDMA partition keys + SAN-equivalent attestation in the QP-establishment ULP." §2.4 says libfabric auth is "per-provider" and lists three different mechanisms (cxi keys, verbs RDMA-cm, EFA security groups). §5.1 hand-waves all of this as "the contract layer's SAN canonicalization runs as an application-layer check after the libfabric connection is established, regardless of provider."

This isn't a security model; it's a list of substitutes. Critical questions go unanswered:

1. **What attestation actually carries the cert SAN to the server in cxi mode?** libfabric/cxi auth keys are pre-shared; they bind a connection to an authentication group, not to a tenant's x.509 cert. If the cert SAN is checked "application-layer" after connection establishment, the connection is already authenticated to the operator's cluster but not to a tenant — what does the server do with the bytes between accept and the first SAN-carrying message? Drop them silently? Reject with what error?
2. **What stops a malicious tenant from spoofing another tenant's SAN over a libfabric/cxi connection that was authenticated only at the auth-key layer?** The auth key is per-cluster, not per-tenant. The SAN is in the application payload. Without an attestation that binds the SAN to a verifiable key on connection establishment, this is "trust the client to tell us its tenant_id" — a model kiseki explicitly rejected in I-NG1.
3. **For mTLS-over-RDMA**: which provider+kernel combinations actually support it? rdma-cm has TLS extensions in newer kernels but compatibility is uneven. Is the ADR committing to a feature that's only in mainline kernels post-6.x?

I-NG1 invariant ("cert SAN URI is the source of truth for tenant identity") needs a provider-by-provider verification matrix or it doesn't hold across bindings.

**Evidence**: §2.3 second paragraph: "OR (where the operator's compliance posture permits) implicit auth via RDMA partition keys + SAN-equivalent attestation". "SAN-equivalent attestation" is undefined. §2.4 third paragraph: "regardless of provider" — but the trust anchor is precisely what differs across providers.

**Suggested resolution**: Architect produces a per-provider trust-anchor table:

| Binding/provider | Connection-level auth | SAN-carrying mechanism | Compliance mode |
|---|---|---|---|
| ibverbs (IB) | rdma-cm + IPsec / TLS-over-RDMA-CM | mTLS extension carries cert | Equivalent to TCP-framed |
| ibverbs (RoCE) | rdma-cm + IPsec / TLS | mTLS extension carries cert | Equivalent to TCP-framed |
| libfabric/verbs | same as ibverbs | same as ibverbs | Equivalent to TCP-framed |
| libfabric/cxi | cxi auth keys (cluster-scoped) | first message carries signed SAN attestation | NOT equivalent to mTLS — requires attestation-key infrastructure |
| libfabric/efa | EFA security groups + AWS IAM | signed token in first message | AWS-IAM-equivalent |

For non-mTLS-equivalent providers (cxi, efa, sockets-fall-back), the ADR must specify the attestation mechanism — most likely a signed-by-cluster-key envelope that the keymanager validates. Until this is pinned, the RDMA bindings cannot ship without weakening I-NG1.

If pinning the cxi attestation story is too expensive for this ADR, defer the cxi/efa providers to a follow-up ADR and ship libfabric only for the verbs provider (which inherits the ibverbs/mTLS story). The TCP-framed binding already covers commodity datacenter; cxi can come later with proper trust-anchor design.

---

### H2: Binding-selection probe is described as "fully sequential" but the consequences for cross-binding race conditions aren't specified

**Severity**: High
**Category**: Robustness > Concurrency
**Location**: ADR-042 §3.1, §3.4, §15 hot spot 9

**Description**: §15 says "probe is fully sequential". §3.1 says the runtime "spawns a listener for every Available binding." §3.4 covers crash mid-flight ("removes the binding from topology, bumps version, backoff-restart"). What's missing:

1. **Probe-time port conflicts**: every binding has its own listen address (`KISEKI_NATIVE_GRPC_ADDR` 9100, `KISEKI_NATIVE_TCP_ADDR` 9101, …). What if an operator misconfigures and two bindings end up on the same port? §3.1 doesn't say what happens — first listener wins? Both fail? Server refuses to start?
2. **Partial probe success**: ibverbs probe `Available`, listener fails to bind (e.g. another process holds the rdma device). What state does the runtime end up in? The §3.1 sequence says "spawn a listener for every Available binding" but doesn't differentiate between probe (read-only check) and listener-spawn (resource acquisition that can fail independently).
3. **Probe time bounds**: `dlopen` of `libfabric.so` + `fi_getinfo()` enumeration can take seconds on a heavily loaded host. Is probe blocking startup? Does `libfabric_probe()` have a timeout? What if `fi_getinfo()` hangs (it has been known to)?
4. **Probe in PID 1 / containerized environments**: kubernetes pods often have limited `/sys` visibility. If `/sys/class/infiniband/*` is masked but `libibverbs.so` is loadable, the probe self-disqualifies cleanly per the spec. But what about the inverse — `/sys/class/infiniband/*` populated by a host-mount but `libibverbs.so` is missing inside the container image? Behavior undefined.

**Evidence**: §3.1 enumerates 4 steps but treats listener-spawn as atomic with probe. §3.4 only covers post-startup crashes, not startup-time failures.

**Suggested resolution**: Architect splits the §3.1 sequence into **three** explicit phases with defined error handling at each:

```
Phase 1 (probe): for each compiled binding, call probe() with a 5 s timeout.
                 On Available, record (binding, latency_class, listen_addr).
                 On Unavailable or timeout, log and skip.

Phase 2 (port-conflict check): assert all recorded listen_addrs are distinct.
                                On collision: log, refuse to start, exit code 2.

Phase 3 (listener-spawn): for each Available binding, spawn its listener.
                          On bind() failure for a single binding, downgrade to Unavailable
                          and continue (do NOT fail the whole server unless all bindings
                          fail, which is exit code 3).
```

Add §3.5 documenting probe-timeout semantics and `dlopen` contention behavior. The implementer follows the explicit phasing.

---

### H3: Heterogeneous-cluster topology consistency requires a per-node-binding contract that isn't defined

**Severity**: High
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §3.3 (heterogeneous deployments), §6 (topology discovery), §15 hot spot 10

**Description**: §3.3 and §6 promise "the topology advertises which bindings each node serves and the client picks the highest-ranked one mutually supported." §15 hot spot 10 calls this out as needing a verification, but the spec text doesn't define:

1. **What field on `TopologyInfo` carries the per-node binding list?** §6 mentions `NodeBindings` but it's not in §1's contract types. Field shape, ordering, encoding (postcard vs. proto-derived) — none specified.
2. **What happens if node A advertises "I serve cxi" but the client probes its local environment and decides cxi is unavailable for it?** §3.2 says "the client's preferred binding is the highest-ranked latency_class mutually supported by (a) the local environment, (b) every node the client must dial." But "every node the client must dial" implies a session-scoped uniform binding. What if the client must dial nodes A (cxi-only) and B (tcp-only) within the same operation? Does the client open both connections with their respective bindings? Does it fall back to the lowest common denominator (gRPC) for the entire session?
3. **What if a node updates its binding set (e.g. operator adds an RDMA NIC and restarts)?** Topology version bumps on shard-leader change and split/merge. Does it bump on binding-set change? Spec doesn't say.
4. **Trust at the binding boundary**: if node A serves both gRPC and cxi, and the client connects via gRPC, can it reach a tenant's data that's only readable via cxi (none in current design)? More usefully: do `topology_version` advertisements differ per binding? If a client polled a node via gRPC and learned topology version 7, then later dialed cxi to a different node and got version 5, what's the right resolution? §6 says topology is "eventually consistent within one heartbeat" — that bound applies per binding or globally?

**Evidence**: §3.3 mentions "client picks the best mutually-supported binding for each direct dial" suggesting per-dial selection; §3.2 says "preferred binding... mutually supported by every node the client must dial" suggesting session-scoped selection. The two contradict for the multi-node-multi-binding case.

**Suggested resolution**: Architect adds §6.1 (per-node binding advertisement) defining:

- The Rust type carrying per-node bindings: `NodeBindings { node_id, bindings: Vec<BindingEndpoint> }`, where `BindingEndpoint { binding_id, addr, latency_class }`.
- Per-dial binding selection: each client → node connection picks independently. A multi-node operation may use cxi for node A and tcp for node B in parallel. Client's "preferred binding" is per-edge, not per-session.
- Topology version updates on binding-set change: yes, bump version. Add "binding-set update" to the publishers list in §6.
- Topology version is global across bindings (the responding node's view). Client reconciles by always using the highest seen version across all responses.

This needs to land before phase 4 (TCP-framed binding) implementation; otherwise heterogeneous deployments have undefined behavior.

---

### H4: Per-binding wire-format-version interactions are unspecified for cross-binding sessions

**Severity**: High
**Category**: Correctness > Semantic drift
**Location**: ADR-042 §13.1 (schema discipline), §15 hot spot 12 (mixed-binding session hazards)

**Description**: §13.1 says "TCP-framed and RDMA bindings carry their own version byte at the wire level... per-binding version bumps are independent of contract Rust-type changes". §15 hot spot 12 mentions "a client doing a multipart upload that switches binding mid-session" but only confirms request_id + idempotency_key carry across.

The cross-binding-version scenario is unaddressed:

1. **Client speaks gRPC binding v1 to node A; node B (after failover) only speaks TCP-framed binding v2 with a wire-format change** that, say, alters the `RpcEnvelope` shape (added a field). The client's TCP-framed implementation is at v1; can it talk to a v2 node? §13.1 leaves "per-binding version bumps are independent of contract Rust-type changes" — but the contract types might have changed *too*, and the gRPC-to-TCP-framed handover crosses both.
2. **Server-side**: a node may serve gRPC v1 + TCP-framed v1 simultaneously. Operator upgrades, the new binary serves gRPC v1 + TCP-framed v2. During the rolling upgrade, half the cluster speaks gRPC+TCP-framed-v1, the other half speaks gRPC+TCP-framed-v2. Cross-binding session integrity depends on client-side uniform handling.
3. **Mid-flight topology refresh** during a binding-version bump can flip a session's preferred binding from v1 to v2 between two requests. The client's per-binding connection pool needs to handle "the binding I had is at v2 now, my cached connection at v1 is stale."

This is exactly the kind of failure that surfaces during a rolling upgrade and bricks every long-running session.

**Evidence**: §13.1 final paragraph defers this to "after we declare 1.0" — but the ADR explicitly ships multiple bindings concurrently *now*, and the cross-binding rolling-upgrade case can hit before 1.0.

**Suggested resolution**: Architect adds §13.2 (rolling upgrade discipline):

- Each binding's wire version follows semver-shaped rules: incompatible changes bump the major; backwards-compatible additions don't bump at all (postcard is non-self-describing, so there's no "field absent" tolerance — every change is incompatible at the wire layer).
- A binding's wire version is independent of the contract Rust-type version. But: when the contract Rust types change incompatibly, every binding's wire version bumps in lockstep (same release).
- Rolling upgrade discipline: cluster stays on (binding × version) pairs N and N+1 for the duration of the upgrade. Old clients see new servers via the old version's wire shape; new clients see old servers via the same. This requires backwards-compatibility-shim code in the binding implementations during rolling-upgrade windows.
- Pre-1.0: state explicitly that rolling upgrades are NOT supported. Operators stop the cluster, upgrade all nodes, restart. No cross-version sessions. This is consistent with ADR-022 rev-2/3/4's "wipe + re-replicate" stance.

The pre-1.0 explicit "no rolling upgrade" stance is the architect's cheap out and probably the right one. State it prominently, not just implicitly.

---

## MEDIUM

### M1: dlopen of system libraries opens a supply-chain attack surface that the ADR doesn't address

**Severity**: Medium
**Category**: Security > Trust boundaries
**Location**: ADR-042 §2.3 (ibverbs probe), §2.4 (libfabric probe)

**Description**: The probe sequence calls `dlopen("libibverbs.so")` and `dlopen("libfabric.so")` at startup. `dlopen` resolves via `LD_LIBRARY_PATH` + system loader rules. An attacker with write access to a directory ahead of the system path on `LD_LIBRARY_PATH` can substitute a malicious `.so`. The malicious library can intercept every RDMA call, including the bytes the gateway sends/receives.

The gateway runs as a privileged service; its `LD_LIBRARY_PATH` is operator-controlled but not defended in the ADR. Compare to the gRPC binding, which links its TLS dependencies statically (Rust + rustls) and has no dlopen surface.

This is a real concern for HPC environments where shared `LD_LIBRARY_PATH` for module-loading is common (Lmod, Spack, etc.).

**Suggested resolution**: Architect mandates the implementer use absolute paths derived from a config-time-validated list:

```
KISEKI_NATIVE_IBVERBS_LIB=/usr/lib/x86_64-linux-gnu/libibverbs.so.1
KISEKI_NATIVE_LIBFABRIC_LIB=/usr/lib/x86_64-linux-gnu/libfabric.so.1
```

with default values pinned per supported Linux distribution. Probe verifies the file is owned by root and not group/world-writable. Refuse to dlopen otherwise. Audit-log every dlopen with the resolved absolute path so a path-injection attack leaves a trace.

---

### M2: `KISEKI_NATIVE_TRANSPORT` env-var override is documented but not specified for security-sensitive cases

**Severity**: Medium
**Category**: Security > Trust boundaries / robustness
**Location**: ADR-042 §3.1 (operator override), §3.2 (client-side selection)

**Description**: The env var lets an operator pin a binding for "diagnosis or compliance audit". But:

1. **Server side**: pinning to gRPC for "compliance audit" disables the lower-overhead bindings. If the operator forgets to remove the pin after the audit, performance silently regresses to the gRPC tax forever. No automatic alarm.
2. **Client side**: pinning lets a tenant's client force gRPC (or any other binding). What if the operator's policy is "this tenant must use mTLS-via-TCP-only" but the env var on the client is `=ibverbs`? Spec says "pinning to a binding that no node serves returns `TransportError::PinnedBindingUnavailable`" — but what if the binding *is* served and the pin overrides the operator's intent? There's no server-side enforcement that a tenant *must* use a particular binding.
3. **`auto` semantics**: spec doesn't define what `KISEKI_NATIVE_TRANSPORT=auto` means vs not setting the env var at all. Are they equivalent? Is `auto` the literal string?

**Suggested resolution**:

- Add server-side metric `kiseki_native_binding_pinned_total{binding}` so operators can see pinned-binding deployments at scrape time.
- Define `auto` literal semantics: equivalent to env var unset. Document as the recommended setting.
- For per-tenant binding *requirement* (not just selection), defer to the operations ADR mentioned in A8. Note in §3.1 that env-var pin is operator-side only, not tenant-binding-policy.

---

### M3: SAN canonicalization helper cross-binding handoff has no mechanically-checkable contract

**Severity**: Medium
**Category**: Correctness > Implicit coupling
**Location**: ADR-042 §5.1 (per-binding hook table), §5.2 (per-handler check), §15 hot spot 2

**Description**: Each binding stashes the canonical SAN form in a binding-specific location: tonic request extensions (gRPC), per-connection context (TCP-framed), QP-establishment ULP (RDMA). The handler reads this via the `RequestPrincipal` extractor trait that §15 hot spot 2 mentions but the ADR doesn't define.

If any binding's stash is wrong (e.g. the TCP-framed connection-context is keyed by socket fd instead of connection id, and a connection is reused across tenants in some weird path), the handler reads the wrong principal. SAN canonicalization on the next request would catch it on the second request, but the first request has already proceeded.

**Suggested resolution**: Architect defines `RequestPrincipal` in §1.4:

```rust
trait RequestPrincipal {
    fn cert_san_canonical(&self) -> &str;
    fn binding_id(&self) -> BindingId;
    fn connection_id(&self) -> ConnectionId;  // for audit + correlation
}
```

Each binding's connection hook stashes the canonical SAN once at connection establishment; the binding's request-handler entry point packages it into a `RequestPrincipal` impl and passes it to `ServerImpl`. `ServerImpl` reads only via this trait — no binding-specific code in the handler.

Adds a unit test in the contract layer: `RequestPrincipal` round-trip per binding, asserting the same SAN survives. Implementer adds one BDD scenario per binding asserting tenant_id mismatch still rejects.

---

### M4: Per-binding listener crashes can race with topology version updates

**Severity**: Medium
**Category**: Robustness > Failure cascades
**Location**: ADR-042 §3.4

**Description**: When a binding's listener crashes:
1. Runtime removes the binding from topology advertisement.
2. Bumps `topology_version`.
3. Backoff-restart spawns the failed listener.
4. On successful restart, re-advertises and bumps version again.

Between steps 1 and 4 a client may be in the middle of a request that succeeded against the now-crashed listener (the request landed before the crash). The client's response may carry the topology version from *before* step 2 (the listener replied with the old version, then crashed). The client now sees:

- Response says "topology_version = N" (pre-crash).
- Client polls topology, gets "version = N+2" (post-restart).
- Client's cache has both versions; which is canonical?

Per §6 the resolution is "always use highest seen version" — but this conflates "I learned about a binding crash" with "shard leadership changed", and the client can't distinguish. Both look like topology shifts.

**Practical consequence**: minor — the client refreshes once on each version mismatch. The §3.4 spec should call out that binding-restart causes spurious topology refreshes (~one per binding per restart) and that the metric `kiseki_native_topology_refresh_total{reason}` should distinguish reasons.

**Suggested resolution**: §3.4 adds: "binding-restart-induced topology version bumps are visible to clients as routine refreshes; they don't indicate shard movement. Metric `kiseki_native_topology_refresh_total{reason}` carries `reason ∈ {leader_change, split, merge, binding_restart, other}` for ops correlation."

---

### M5: Probe latency_class ranking is hardcoded; doesn't account for actual measured latency

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §3.1 (server-side ranking), §3.2 (client-side ranking)

**Description**: §3 ranks bindings by `latency_class` (RDMA > Low > Standard). But:

1. **Per-NIC variance**: a 1 GbE link with TCP-framed may be slower than gRPC over a 100 GbE link in the same cluster. Ranking by class hides this.
2. **Provider variance**: libfabric/sockets-fall-back is class `Standard` per §2.4, which is the same class as gRPC. Tie-breaker?
3. **Cross-tenant**: tenant A's workload is bandwidth-bound (prefer high-throughput NIC); tenant B's is latency-bound (prefer low-latency NIC). Class-based ranking can't express this.

**Suggested resolution**: Accept hardcoded ranking for v1. Add a §3.6 noting:

- The class-based ranking is deliberately coarse for v1.
- Per-binding latency probes (measured at startup or periodically) are captured as a future-ADR item ("adaptive binding selection").
- For v1, operators with non-default tradeoffs use the env-var pin.

---

### M6: §14 performance budget for RDMA bindings is unfalsifiable

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §14

**Description**: §14 lists `ibverbs` and `libfabric/cxi` with target "hardware-bound" and footnote "kiseki overhead ≤ 10 % of fabric peak". This isn't a number that can fail — "hardware-bound" is by definition not falsifiable. If the implementation gets 1k op/s on RDMA hardware that the fabric supports at 1M op/s, the spec's wording technically holds (1k is bounded by hardware; the hardware is just being used badly).

**Suggested resolution**: Architect requires the implementation phase to deliver:

- A per-binding fabric-peak measurement procedure (e.g. `ib_send_bw` for ibverbs; `fi_bw` for libfabric).
- A perf-gate criterion: kiseki throughput on the RDMA binding ≥ 50 % of fabric_peak measured by the fabric tools, on the same hardware. Below 50 % blocks the gate-2 perf check.
- Until the measurement procedure lands, the §14 row for RDMA bindings says "deferred to phase 10/11; perf gate criterion in implementation ADR" — not a vague "hardware-bound" promise.

---

## LOW

### L1: §11.1 still references "system master key" without confirming the rev-4 fjall migration didn't change the rotation model

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-042 §11.1

**Description**: §11.1 says "master key has an epoch (ADR-007)". The ADR-007 / ADR-002 reference is stable, but ADR-022 rev-2/3/4 changed the storage backend for the keymanager epochs (`keys/epochs.redb` → `keys/epochs/` fjall keyspace). §11.1 doesn't reference the change. Cosmetic; doesn't affect correctness because the rotation grace window is contract-level.

**Suggested resolution**: One-line note in §11.1 referencing ADR-022 rev-3 for the keymanager epochs storage.

---

### L2: §16 build phases list 11 phases; §19 says "phase 1 already implemented"

**Severity**: Low
**Category**: Correctness > Documentation drift
**Location**: ADR-042 §16.1, §19

**Description**: §16.1 enumerates 11 phases with phase 1 = "define contract Rust types". §19 says "Phase 1 (gRPC binding) is already implemented (was the original draft's scope)". The phase-1-now-defines-contract-types statement contradicts the phase-1-is-gRPC-binding-already-done statement.

**Suggested resolution**: Renumber: contract-types-first becomes phase 0 (or phase 1a). Existing gRPC binding becomes phase 1b. New phases (TCP-framed, selector, etc.) are 2..N. §19 reflects the renumbering.

---

### L3: A7/A8 alternatives are correctly rejected but conflate "build-time" and "deployment-time" decisions

**Severity**: Low
**Category**: Correctness > Documentation
**Location**: ADR-042 §18 alternatives

**Description**: A7 says "operator-selected single binding at build time — rejected". This is correctly rejected. But the implementation-side build *does* still gate on system deps (libibverbs-dev, libfabric-dev). If those deps aren't on the build host, ibverbs/libfabric bindings can't compile. So there *is* a build-time decision: "does this build target have RDMA system deps?" The user agreed earlier to require these on the build host, but A7 doesn't reflect that constraint.

**Suggested resolution**: A7 amended: "Operator-selected single binding at build time via cargo features — rejected (auto-detection at runtime is what operators want). Note: the build host MUST have libibverbs-dev + libfabric-dev for the RDMA bindings to compile. This is a build-environment requirement, not a build-time deployment decision."

---

## Cross-cutting observations

### O1: The contract layer is genuinely portable, which is the point

The Rust types in §1 are postcard-friendly, prost-friendly, and would round-trip cleanly through any reasonable codec. The contract surface doesn't leak gRPC-isms. This is the most important property of the rewrite and it holds.

### O2: Per-binding observability isn't called out

Every §10 audit event, §6 metric, etc. is contract-level. The bindings need their own per-connection metrics: `kiseki_native_binding_connections_active{binding}`, `kiseki_native_binding_handshake_failures_total{binding}`, etc. The ADR doesn't enumerate these. Implementer should add them; architect should mention the requirement so they're not forgotten.

### O3: BDD parameterization across bindings is mentioned but not specified

§16.1 phase 7 says "parameterized scenarios — `@grpc`, `@tcp_framed` tags". The BDD harness's tag-filter mechanism (cucumber retry_filter via TagOperation; per memory) exists. But: how does the harness know which bindings to spawn? Does it use the runtime probe, or does it pin per scenario? The spec doesn't say. Defer to BDD-completion-plan but link it from §16.1.

### O4: The contract/binding split closes the door on "let me sneak in a feature only one binding supports"

This is good architectural discipline — every binding implements the same contract. But it forecloses some HPC-specific opportunities (e.g. RDMA atomics for compare-and-swap) that won't have analogues in gRPC. Future ADRs may need to extend the contract with optional verbs that bindings declare support for. Out of scope here; flag for future-architect-self.

---

## Recommendation

**Pass conditional on resolving the 4 HIGH findings before phase 4 (TCP-framed binding) begins.** H1 (RDMA trust anchor matrix) is the highest-blast-radius item — it can't be deferred without weakening I-NG1 across the RDMA bindings. H2 (probe phasing), H3 (per-node binding advertisement), and H4 (rolling-upgrade discipline) are tractable with focused architect amendments.

The 6 MEDIUM findings should land before implementer phase 10–11 (RDMA bindings) but don't block phase 1–9. The 3 LOW findings are documentation polish.

Implementer can begin phase 0 (contract type extraction) immediately. Phase 4 (TCP-framed binding) waits on H2+H3+H4 amendments.
