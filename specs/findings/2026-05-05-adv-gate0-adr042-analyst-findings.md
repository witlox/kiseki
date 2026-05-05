# Adversary Gate-0 Findings — ADR-042 Analyst Output

**Type**: Adversary → Analyst (gate before architect picks up)
**Date**: 2026-05-05
**Reviewer**: adversary (architecture mode)
**Mode**: pre-architect adversarial review. The architect has not drafted ADR-042 yet; this pass is on the analyst's spec-layer artifacts only (`ubiquitous-language.md` additions, `A-NG1..A-NG11`, `I-NG1..I-NG10`, `specs/features/native-gateway.feature`, `F-NG1..F-NG7`, `specs/escalations/2026-05-05-analyst-handoff-adr-042-native-gateway.md`).
**Verdict**: **CHANGES REQUESTED** — 1 CRITICAL, 6 HIGH, 8 MEDIUM, 4 LOW. The CRITICAL and at least 4 of the 6 HIGH must be resolved (in spec or by escalation back to analyst) before architect drafts ADR-042; the rest can resolve via ADR-042 if architect chooses.
**Status (2026-05-05)**: All findings resolved at the analyst layer per the domain expert's "no deferrals — solve all in place" directive. Resolution table at the bottom of this file.

## Summary

| Severity | Count |
|---|---|
| Critical | 1 |
| High     | 6 |
| Medium   | 8 |
| Low      | 4 |

The analyst output is largely well-structured, the wave-based interrogation captured the load-bearing decisions, and the cross-reference back to existing invariants (I-L5, I-L8, I-K8, ADR-031, ADR-035) is thorough. However, **internal inconsistencies** between A-NG6 ("no session state") and I-NG6 ("existing handles see the mode they opened under") will cause architect to make a structural decision the analyst didn't intend, and **multiple security-critical edges** (TrustedCompute crypto-shred, lease split-brain, dedup state durability, SAN URI canonicalization) are unspecified.

---

## CRITICAL findings (block architect)

### F-C1: TrustedCompute mode breaks crypto-shred propagation

**Severity**: Critical
**Category**: Security > Cryptographic correctness, Trust boundaries
**Location**: `specs/assumptions.md` A-NG7; `specs/invariants.md` I-NG6; `specs/ubiquitous-language.md` "Trusted compute pool"
**Spec reference**: ADR-002 (two-layer encryption), ADR-011 (crypto-shred cache TTL), F-K3 (key compromise propagation), F-CC3 (cached plaintext under shred)

**Description**: Under `crypto_boundary = TrustedCompute`, the server returns sealed envelopes plus DEK references and the *client* fetches the DEK from the keymanager and decrypts locally (A-NG7). Once the client has a DEK in process RAM, **there is no mechanism to revoke it**. When a tenant issues a crypto-shred (intentional destruction via tenant-KEK rotation/destruction, F-K3 / F-CC3), every TrustedCompute client that has previously fetched a DEK retains the ability to decrypt the cached envelope bytes for the configured cache lifetime — possibly indefinitely if the client is long-running.

The existing crypto-shred semantics rely on the keymanager refusing further unwraps after revocation. That works because **today** every decrypt happens server-side: the server holds the only copy of the unwrapped DEK and the cache TTL bounds residual exposure (F-CC3 = P1, max 30 s exposure window). Under TrustedCompute, the *client* holds the DEK; F-CC3's mitigation does not transfer. The native client SDK has no equivalent of `kiseki-cache-scrub` for in-process key material, and even if it did, a malicious client could simply ignore the revocation signal.

**Evidence**:
1. ADR-011 / I-K3 / F-CC3 jointly require crypto-shred to take effect within a bounded window (30 s default). F-CC3 calls this out as P1.
2. A-NG7 specifies opt-in client-decrypt without any A-NG entry pinning the crypto-shred contract under TrustedCompute.
3. I-NG6 says "switching modes requires an explicit `Setattr` on the namespace" — the *namespace* mode flip can be observed by the keymanager (which can refuse subsequent DEK fetches), but **already-issued DEKs in client RAM are not affected**.

**Suggested resolution** (minimal):
- Either (a) **reject TrustedCompute as designed** and require client-side decrypt to use a per-read DEK delivered fresh each time (no client-side DEK cache; every read pays a keymanager round-trip — kills most of the perf win), or (b) **add a new A-NG entry and matching invariant** explicitly **scoping** TrustedCompute to namespaces marked `crypto_shred = "best-effort"` (a new namespace property), with explicit operator acknowledgement that crypto-shred is not enforceable for those namespaces, or (c) **add a key-pinning protocol** — TrustedCompute clients must re-fetch the DEK on every chunk read (no in-process caching beyond the single read), eliminating residual exposure but losing the GPU-direct benefit.
- Architect must pick. Spec must capture the choice as A-NG12 + I-NG11 + F-NG8 (DEK-in-client-RAM exposure mode).

---

## HIGH findings (block architect unless escalated)

### F-H1: A-NG6 ("no session state") contradicts I-NG6 ("existing handles see the mode they opened under")

**Severity**: High
**Category**: Correctness > Implicit coupling, Specification compliance
**Location**: `specs/assumptions.md` A-NG6; `specs/invariants.md` I-NG6
**Spec reference**: A-NG6, I-NG6

**Description**: A-NG6 says all per-call control fields are "per-call, no session state, no out-of-band RPCs". I-NG6 says when `crypto_boundary` mode flips on a namespace, "existing handles see the mode they opened under" — which **requires** server-side per-handle state that survives the mode flip. The two are contradictory, and the contradiction is not just semantic: object-flavored verbs are stateless (no "handles"), while POSIX-flavored verbs do have inode handles, and I-NG6 doesn't distinguish.

If the architect resolves this by making per-handle mode genuinely stateless (mode is encoded into the inode handle token returned at open time), then I-NG6 is satisfied without violating A-NG6 — but that's a non-trivial protocol decision, not a free pick.

**Suggested resolution**:
- Split I-NG6 into I-NG6a (object verbs: mode flip is immediately observable; no in-flight handles to honor) and I-NG6b (POSIX verbs: mode is captured in the open-handle token; the token carries the mode; flips only affect the next open).
- Or strike the "existing handles see old mode" clause and accept that mode flips are immediately observable everywhere — POSIX clients with active handles must re-open. Simpler. Less surprising. Document the consequence in F-NG (new entry).

### F-H2: Idempotency dedup state survival on leader change unspecified

**Severity**: High
**Category**: Correctness > Concurrency, Specification compliance
**Location**: `specs/invariants.md` I-NG5; `specs/assumptions.md` A-NG10; `specs/failure-modes.md` F-NG3
**Spec reference**: I-NG5

**Description**: I-NG5 promises the server "deduplicates by `(tenant_id, namespace_id, idempotency_key)` over a 5-minute window". F-NG3 (leader change mid-stream) says the client retries with the same key and the request "commits exactly once". But **nowhere does the spec say where the dedup state lives** — is it leader-local in-memory, leader-local persistent, or Raft-replicated to followers?

If leader-local-in-memory: every leader change loses the dedup window, and a retried write within the window can double-apply. I-NG5 is then false.

If leader-local-persistent (e.g., redb): leader change loses the dedup window unless the new leader can read it from the previous leader's local store — which it can't (different node).

If Raft-replicated: dedup state replication doubles the Raft proposal cost per write, undermining A-NG11 perf target.

**Suggested resolution**:
- Architect picks the dedup state's persistence model in ADR-042.
- Recommend Raft-replicated dedup-state-as-payload-side-effect: each accepted write's apply phase records `(tenant_id, namespace_id, idempotency_key, original_response)` in a TTL-bounded redb table on every voter. Cost is a few bytes per write, well below a doubled Raft round-trip. Architect to validate.
- If the architect picks leader-local, A-NG10's 5-min window claim must be downgraded to "best-effort within a leader epoch" and F-NG3 must explicitly accept double-apply on leader change as a known outcome.

### F-H3: SAN URI canonicalization is undefined

**Severity**: High
**Category**: Security > Trust boundaries, Cryptographic correctness
**Location**: `specs/invariants.md` I-NG1; `specs/assumptions.md` A-NG2; `specs/ubiquitous-language.md` "Cert-SAN tenant binding"
**Spec reference**: I-NG1

**Description**: I-NG1 says the cert SAN URI must match the payload `tenant_id` "byte-exact on the canonical SAN URI form (`spiffe://kiseki/tenant/<org_id>` or equivalent)". The canonicalization rules are not specified. Concrete attacks:

1. **Trailing slash**: `spiffe://kiseki/tenant/org-a` vs `spiffe://kiseki/tenant/org-a/`. RFC 3986 says these are different URIs, but a casual implementer may strip trailing slashes.
2. **Case in scheme/host**: `SPIFFE://kiseki/tenant/org-a` vs `spiffe://kiseki/tenant/org-a`. RFC 3986 says scheme is case-insensitive, host is case-insensitive — byte-exact would reject both as different from the canonical form.
3. **Percent-encoding**: `spiffe://kiseki/tenant/org%2Da` vs `spiffe://kiseki/tenant/org-a`. RFC 3986 says they're equivalent, byte-exact rejects.
4. **Unicode normalization**: tenant id with combining characters (e.g., NFC vs NFD form). Byte-exact rejects, but spoofing-relevant.
5. **IDN / Punycode**: `spiffe://кiseki/tenant/org-a` (Cyrillic к) vs `spiffe://kiseki/tenant/org-a` (Latin k). Byte-different, visually identical. Homograph attack.

Without specifying canonicalization, an implementer's choice **becomes** the security boundary, and a future implementation drift causes silent forgery.

**Suggested resolution**:
- Specify canonicalization rules explicitly in I-NG1: scheme lowercased, host lowercased, no trailing slash, percent-encoding decoded for unreserved characters, NFC for tenant id, ASCII-only (reject IDN by hard rule).
- Add an A-NG entry capturing the rules.
- Add a Gherkin scenario that asserts a near-miss (e.g., trailing slash) is rejected.

### F-H4: Lease split-brain on network partition has no fencing

**Severity**: High
**Category**: Correctness > Concurrency, Failure cascades
**Location**: `specs/invariants.md` I-NG10; `specs/failure-modes.md` F-NG4
**Spec reference**: I-NG10

**Description**: I-NG10 grants an exclusive-write lease with a TTL. On lease holder death (network partition or crash) the server expires the lease and grants it to a new holder (F-NG4). But during partition + heal, the **old holder may not have observed the expiry** — they think they still hold the lease. They issue a write. The server has already granted the lease to a new holder; now two writers believe they have exclusivity.

POSIX systems traditionally solve this with **fencing tokens** (monotonically-increasing sequence numbers in every lease-holding write; the server rejects writes with a stale token). The spec doesn't specify fencing.

**Evidence**:
1. I-NG10 specifies TTL but not how the *write path* checks lease validity. If writes only check "did the lease exist when we accepted it?", a partitioned old holder's write succeeds.
2. F-NG4 says "uncommitted writes on the dead lease are invalidated server-side" — but this only catches *server-staged* writes, not new writes issued by the partitioned old holder during partition heal.

**Suggested resolution**:
- Add to I-NG10: "Every write issued under a lease carries the lease's monotonic fencing token. The server rejects any write whose fencing token is older than the current lease's token."
- Add Gherkin scenario for partition-heal split-brain with fencing rejection.
- Cross-reference Lamport's fencing-tokens pattern in the architect handoff.

### F-H5: TrustedCompute namespace flag flip mid-flight is not actually addressed

**Severity**: High
**Category**: Security > Trust boundaries
**Location**: `specs/invariants.md` I-NG6; `specs/assumptions.md` A-NG7
**Spec reference**: I-NG6

**Description**: I-NG6's clause "switching modes requires an explicit `Setattr` on the namespace and only takes effect for new opens" is internally fine for POSIX verbs (open-handle scoped) but problematic when interpreted with object verbs:

1. Object verbs are stateless. Every Read is a "fresh open." So a flip from `TrustedCompute` → `ServerOnly` is immediately observable on the **next** Read — no in-flight protection.
2. But the *previous* Read may have just returned a sealed envelope to a client that's about to fetch the DEK. The DEK fetch races with the namespace flag flip. If the keymanager honors the flag flip (refuses the DEK), the client cannot decrypt — and the client doesn't know whether to retry. If the keymanager honors the namespace's flag at *DEK-fetch time* rather than *Read time*, the client gets the DEK and decrypts, but a tenant who just flipped the flag to seal off external decryption sees old reads still decrypting.

**Suggested resolution**:
- Specify which moment the flag is evaluated for the DEK fetch: **at the moment of the Read**, captured as a hint in the response and validated by the keymanager when the client presents it.
- Add A-NG12 capturing the DEK-fetch race policy.
- Add an I-NG entry: "DEK fetch under TrustedCompute requires the keymanager to verify the namespace was in TrustedCompute mode *at the moment of the corresponding Read*; subsequent flag flips do not retroactively block the DEK fetch."

### F-H6: No bound on concurrent in-flight streaming writes per tenant

**Severity**: High
**Category**: Robustness > Resource exhaustion
**Location**: `specs/failure-modes.md` F-NG2; `specs/invariants.md` I-NG2 / I-NG9
**Spec reference**: F-NG2

**Description**: I-NG9 caps a single stream at 64 MiB. F-NG2 says server-side staging is "bounded by the per-stream cap (64 MiB)". But **nothing bounds the number of concurrent streams** per tenant. A malicious or buggy tenant could open 10 000 concurrent streams, each up to 64 MiB, for 640 GB of server-side staging — well beyond any node's RAM.

The mitigation should be a per-tenant in-flight-stream limit, exposed as a config knob and enforced at the proto-handler boundary.

**Suggested resolution**:
- Add I-NG11 (or extend I-NG9): "The server enforces a per-tenant cap on concurrent in-flight streaming writes (default 256). Excess `OpenStream` requests are rejected with `ResourceExhausted`."
- Add F-NG (new entry) capturing the malicious-tenant flood vector.
- Architect picks the default and the override path (per-tenant policy via control plane).

---

## MEDIUM findings (architect should resolve in ADR-042; not blocking)

### F-M1: Audit-on-rejection at proto boundary requires explicit wiring

**Severity**: Medium
**Category**: Security > Observability gaps
**Location**: `specs/escalations/2026-05-05-analyst-handoff-adr-042-native-gateway.md` ("auto-fires through GatewayOps"); F-NG1
**Spec reference**: I-NG7

**Description**: F-NG1 (cert/payload mismatch) describes a security-failure audit event emitted *before* any gateway work runs. But the analyst handoff claims "no new audit wiring" because "the gateway data endpoint dispatches into the same `GatewayOps` trait that S3/NFS/FUSE use". A request rejected at the proto boundary **never reaches `GatewayOps`** — so the auto-fire claim does not cover this case. New audit-emit wiring is needed at the proto-handler boundary.

**Suggested resolution**: Architect note in ADR-042 that proto-boundary rejections need their own audit-emit hook. Update I-NG7 to specify both the gateway-dispatched and the proto-boundary-rejected cases.

### F-M2: Read-after-fsync from a different node not addressed

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: `specs/invariants.md` I-NG3
**Spec reference**: I-NG3, ADR-026 (Strategy A — readers can hit any voter)

**Description**: I-NG3 says POSIX writes become visible "after `Fsync` returns Ok". But Fsync hits the leader; followers may not have applied the corresponding delta yet. A reader on a follower issued immediately after the writer's Fsync may see stale state. POSIX expects read-your-writes from the same caller, but clients may dial different nodes per call (especially under hybrid leader routing).

**Suggested resolution**: Either (a) follower reads of just-fsync'd inodes must wait for catch-up before returning (more latency), or (b) clients are required to dial the leader for read-after-write consistency on POSIX verbs (forces topology cache freshness). Architect picks; document in I-NG3.

### F-M3: Hybrid routing topology cache invalidation only via TTL

**Severity**: Medium
**Category**: Correctness > Failure cascades
**Location**: `specs/invariants.md` I-NG8
**Spec reference**: I-NG8

**Description**: I-NG8 says client topology cache TTL is 30 s. So every leader change can produce up to 30 s of LeaderUnavailable thrash for clients with a stale cache. A push-based invalidation (server includes `topology_version` in every response; client refreshes when version increments) is cheap and reduces the thrash to a single-RTT amortized.

**Suggested resolution**: Add `topology_version` field to all responses; client refreshes topology cache on version mismatch. Update I-NG8.

### F-M4: A-NG11 performance target without an in-process floor measurement

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: `specs/assumptions.md` A-NG11
**Spec reference**: A-NG11

**Description**: A-NG11 sets the per-node native target at ≥80 k op/s (64 KiB GET) based on the strategic framing "between Lustre-DoM and VAST" and a guess that "gRPC tax ≈ 30 % vs. in-process gateway floor". The in-process floor is **not measured** — the analyst deferred that test in favor of the strategic discussion. If the in-process floor is 30 k op/s (gateway internals are the binder, not the wire), then the 80 k target is unreachable regardless of protocol.

**Suggested resolution**: Run an in-process driver experiment **before** ADR-042 lands. If the floor is ≥ 100 k op/s, A-NG11's 80 k target is plausible. If the floor is <50 k op/s, A-NG11 must be revised down or the gateway-internal hot-path cleanup must be prioritized as a precursor to ADR-042.

### F-M5: Object verbs vs POSIX verbs for streaming-vs-unary boundary

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: `specs/invariants.md` I-NG2 / I-NG3 / I-NG9
**Spec reference**: I-NG9

**Description**: I-NG9 says the streaming threshold is the inline-threshold (8 KiB). But:
- Object writes < 8 KiB: unary, commit-on-close (trivially atomic).
- Object writes ≥ 8 KiB: streaming, commit-on-close.
- POSIX writes via inode handle: are they always streaming (open + write + ... + fsync)? Or does a single `Write(inode, offset, bytes)` use unary if bytes ≤ 8 KiB?

The Gherkin scenarios don't cover this case. The architect must decide whether POSIX `Write(ino, offset, bytes)` shares the streaming threshold or always streams.

**Suggested resolution**: Clarify I-NG9 to specify the boundary for both verb families. Add Gherkin scenarios for both cases.

### F-M6: Lease + drain interaction unspecified

**Severity**: Medium
**Category**: Correctness > Failure cascades, Cross-context
**Location**: `specs/invariants.md` I-NG10; ADR-035 (drain protocol)
**Spec reference**: I-NG10, A-N4 (drain refusal)

**Description**: ADR-035 / I-N4 says drain refuses if completing the drain would drop a shard below RF=3. But what if the draining node hosts an active POSIX lease holder? Two options:

1. Drain quiesces by waiting for all leases on the node to expire / release. Drain may take up to lease TTL (30 s default) longer.
2. Drain forcibly revokes leases and requires holders to re-acquire on a different node. Visible to the client.

Either is fine; the spec doesn't say.

**Suggested resolution**: Add an I-NG or A-NG entry capturing the choice. Update F-NG4 to mention drain-induced lease revocation.

### F-M7: Workflow_ref required vs optional on writes

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: `specs/ubiquitous-language.md` "Cache hint" (passing reference); A-NG6
**Spec reference**: A-NG6, ADR-020/021 (workflow advisory)

**Description**: A-NG6 says workflow_ref is a per-call field. But the spec doesn't say:
- Is it **required** on every write? (I-WA* may require, depending on tenant policy.)
- What's the **default** when absent? Empty string? `unknown-workflow`? Reject?
- Is it **immutable** within an idempotency-key dedup window? (If the original carried `wf-A` and a retry carries `wf-B`, is that a duplicate? Architect must choose.)

**Suggested resolution**: Specify in I-NG (new) or A-NG7 amendment. Recommend: required when tenant policy mandates (per ADR-020), idempotency-key dedup is workflow_ref-blind (key is the dedup primitive).

### F-M8: Connection multiplexing model mostly punted to architect

**Severity**: Medium
**Category**: Correctness > Specification compliance
**Location**: Architect handoff open question 2
**Spec reference**: Handoff Q2

**Description**: The analyst handoff defers connection multiplexing to architect, which is correct. But the spec already commits to certain shapes that constrain it:
- HTTP/2 multiplexing implies one channel can carry many concurrent streams. A-NG6 ("no session state") is friendly to that.
- I-NG8 hybrid routing implies each shard's leader is a separate dial target. So a client may need N channels for N leaders.

These constraints aren't captured anywhere. Architect could pick a model that violates them.

**Suggested resolution**: Add an A-NG entry capturing the implications: "client maintains one channel per node it currently believes to be a shard leader; channel is reused for all shards led by that node; channel is closed when the node is no longer a leader for any shard the client cares about."

---

## LOW findings

### F-L1: Gherkin scenario hard-codes default lease TTL of 31 s

**Severity**: Low
**Category**: Correctness > Edge cases
**Location**: `specs/features/native-gateway.feature` "Native POSIX lease — holder dies, lease expires"
**Spec reference**: I-NG10

**Description**: The scenario says "When 31 seconds elapse" assuming default TTL=30s. If the default changes (or the scenario runs under a non-default config), it fails. Should be parameterized: "after the configured lease TTL expires".

**Suggested resolution**: Rewrite as `When the configured lease TTL elapses without a RenewLease`.

### F-L2: `@native @routing` Gherkin uses OR in the Then clause

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: `specs/features/native-gateway.feature` "Native client falls back via server-side proxy on stale topology"
**Spec reference**: I-NG8

**Description**: The Then has "node-2 responds with NotLeader{leader=node-3} OR proxies the request to node-3". Gherkin scenarios should assert one specific behavior. "OR" makes the test pass under either outcome, but they're functionally different — one returns to the client, the other transparently completes.

**Suggested resolution**: Split into two scenarios, one for each branch. Architect chooses the steady-state behavior in ADR-042.

### F-L3: Idempotency key length cap (≤64 bytes) is not an invariant

**Severity**: Low
**Category**: Correctness > Edge cases
**Location**: `specs/ubiquitous-language.md` "Idempotency key" (only)
**Spec reference**: I-NG5

**Description**: The 64-byte cap is in the term definition but not in I-NG5. A buggy server could accept arbitrary-length keys and a malicious client could exhaust dedup-state memory faster than F-H6 mitigation expects.

**Suggested resolution**: Add to I-NG5: "Keys longer than 64 bytes are rejected with `InvalidArgument` at the proto-handler boundary."

### F-L4: `@native @perf @smoke` scenario asserts strict zero errors

**Severity**: Low
**Category**: Correctness > Edge cases
**Location**: `specs/features/native-gateway.feature` "Native object 64 KiB GET — per-node throughput target"
**Spec reference**: A-NG11

**Description**: "no errors are reported" in a 30-second 16-worker test is unrealistic — under load, transient TCP timeouts, GC pauses, etc. produce 1-in-N errors that don't indicate a regression. The existing matrix accepts this; the BDD scenario should too.

**Suggested resolution**: Change to "the error rate is below 0.01 % (≤ ~1 in 10 000 ops)".

---

## Gaps / missing failure modes

The following are NOT findings against the existing artifacts but observations that the architect or analyst should consider adding to the spec:

- **F-NG (missing): cert revocation mid-session**. A long-running streaming write whose cert is revoked partway through. Server should periodically re-validate the cert on long-lived streams. Period unspecified.
- **F-NG (missing): clock skew between client and server**. Lease arithmetic depends on clock agreement. The spec assumes NTP-synced clocks (cf. existing time invariants), but doesn't pin a tolerance.
- **F-NG (missing): replay attack via captured idempotency_key after cert rotation**. The cert SAN binds tenant identity, but if cert A is rotated and an attacker captured cert A + a request, can they replay? mTLS prevents the TCP handshake, but cert validity windows matter. Out of scope for ADR-042 if mTLS termination is per-server-side; capture as A-NG entry.
- **F-NG (missing): server-side proxy fallback (I-NG8 hybrid) — proxying node fails mid-proxy**. Client doesn't know if write committed. Same idempotency-key retry path applies but worth calling out.

---

## Cross-cutting observations

1. **The analyst output is internally inconsistent in two places** (F-H1 stateless-vs-stateful, F-H2 dedup persistence). Both are resolvable in ADR-042 but require explicit architect decisions; the analyst should not have shipped without flagging them.

2. **TrustedCompute mode (Q7=c) is the largest source of unresolved questions** (F-C1, F-H5, plus the missing F-NG entries on DEK exposure). The analyst captured the *decision* but not the *threat model*. Architect should consider whether TrustedCompute is in scope for the first ADR-042 cut, or if it's a follow-up ADR (042-trusted-compute) — the rest of the design holds without it.

3. **Performance target A-NG11 is on a strategic basis, not a measured one** (F-M4). Recommend running the in-process floor measurement before architect commits to a proto shape that has to hit 80 k op/s.

4. **Hybrid leader routing (I-NG8) and lease semantics (I-NG10) are the two operationally-complex pieces** that interact with ADR-035 (drain) and ADR-026 (per-shard Raft). Architect should walk through every state combination; F-M6 + F-H4 are the start of that exercise.

5. **The verdict is CHANGES REQUESTED, not REJECTED** — the analyst output is structurally usable. The 1 CRITICAL + 6 HIGH must be resolved (in spec or by escalation back to analyst) before architect drafts ADR-042; the 8 MEDIUM and 4 LOW can resolve via ADR-042 if architect chooses.

---

## Recommended next steps

1. **Analyst**: address F-C1 (TrustedCompute crypto-shred) — pick option (a)/(b)/(c) from F-C1's resolution suggestions. This is the only finding that *might* require re-running an interrogation wave with the domain expert.

2. **Analyst**: address F-H1 (stateless vs stateful) — resolve A-NG6 / I-NG6 contradiction. Likely a spec-text edit, not a new wave.

3. **Analyst**: address F-H3 (SAN canonicalization) — write canonicalization rules into I-NG1. Spec-text edit.

4. **Analyst** (optional): address F-H6 (concurrent stream cap) and F-H5 (TrustedCompute flag flip race) at spec layer or punt to architect.

5. **Architect**: pick up after those land. F-H2 (dedup persistence) and F-H4 (lease fencing) are correctly architect-level.

6. **Pre-ADR-042 perf experiment**: run the in-process driver to validate A-NG11 before the protocol shape commits.

---

## Resolution table (2026-05-05, all findings addressed in place)

| Finding | Resolution location |
|---|---|
| F-C1 (TrustedCompute crypto-shred) | New term *crypto-shred policy* in `ubiquitous-language.md`; A-NG7 amended to require `crypto_shred_policy = best_effort` when `crypto_boundary = TrustedCompute`; new I-NG6c invariant rejects the configuration if one is set without the other; Gherkin scenarios "TrustedCompute requested without best-effort shred — rejected" and "TrustedCompute with explicit best-effort shred — accepted" added to `native-gateway.feature`. |
| F-H1 (A-NG6 vs I-NG6 contradiction) | I-NG6 split into I-NG6a (object verbs: immediate observability, DEK-fetch ticket carries at-Read-time mode) and I-NG6b (POSIX verbs: handle token carries open-time mode statelessly). A-NG6 amended to make handle-token mechanism explicit. |
| F-H2 (idempotency dedup persistence) | A-NG10 amended to specify Raft-replicated dedup state as a side-effect of the apply phase. I-NG5 updated to match. |
| F-H3 (SAN URI canonicalization) | New term *Canonical SAN URI form* in `ubiquitous-language.md` with explicit rules; I-NG1 updated to reference the canonical form; Gherkin Scenario Outline "Native PUT — SAN URI near-miss is rejected" with 6 examples. |
| F-H4 (lease fencing token) | New term *Fencing token* in `ubiquitous-language.md`; A-NG12 added; I-NG10 amended to require fencing token; new I-NG12 invariant; Gherkin scenario "Native POSIX lease — partition-heal split-brain rejected by fencing". |
| F-H5 (TrustedCompute flag flip race) | I-NG6a explicitly addresses this: keymanager validates DEK fetch against the namespace mode at the moment of the corresponding Read; the Read response carries an opaque DEK-fetch ticket whose contents commit to the at-Read-time mode. |
| F-H6 (concurrent stream cap) | A-NG14 added (default 256 streams per tenant); new I-NG11 invariant; Gherkin scenario "Per-tenant concurrent stream cap enforced at proto boundary". |
| F-M1 (audit-on-rejection wiring) | I-NG7 amended to specify both the gateway-dispatched and the proto-boundary-rejected audit paths. |
| F-M2 (read-after-fsync from a different node) | A-NG16 added (POSIX read-your-writes routes through leader until the lsn-of-last-fsync is observed on the follower); I-NG3 amended to reflect this. |
| F-M3 (topology cache invalidation push-based) | New term *Topology version* in `ubiquitous-language.md`; A-NG13 added; new I-NG13 invariant; Gherkin scenario "Topology cache refreshed on topology_version mismatch". |
| F-M4 (perf target without measured floor) | A-NG11 amended to make the in-process floor measurement a graduation gate; the analyst handoff escalation includes this as an open `[ ]` checklist item before architect drafts ADR-042. |
| F-M5 (object/POSIX streaming boundary) | I-NG9 amended to specify the streaming threshold for both verb families. |
| F-M6 (lease + drain interaction) | New I-NG14 invariant. Gherkin scenarios "Drain waits for outstanding leases to expire" and "New AcquireLease against a draining node — rejected". |
| F-M7 (workflow_ref required vs optional) | A-NG6 amended to specify required-when-tenant-policy-mandates and default-when-absent (`unattributed`). |
| F-M8 (connection multiplexing model) | A-NG15 added (one channel per node-believed-leader; HTTP/2 multiplexed across shards). |
| F-L1 (Gherkin hardcoded TTL) | "Native POSIX lease — holder dies, lease expires" rewritten to use "the configured lease TTL plus the renewal grace window". |
| F-L2 (OR in Then clause) | Original scenario split into "Native client receives NotLeader and refreshes topology" + "Native client transparently uses server-side proxy fallback" — two scenarios, each with one specific behavior. |
| F-L3 (idempotency key length not invariant) | I-NG1 amended to validate idempotency-key length at the proto-handler boundary; I-NG5 reaffirms 1..=64 bytes. |
| F-L4 (smoke scenario zero-error too strict) | Smoke scenario error-rate threshold relaxed to "≤ 0.01% (≤ ~1 in 10 000 ops)". |
| Missing F-NG (cert revocation mid-session) | A-NG17 added; F-NG8 added; Gherkin scenario "Long-running stream torn down on cert revocation". |
| Missing F-NG (clock skew) | A-NG18 added; F-NG9 added; Gherkin scenarios "Clock skew within tolerance" and "Clock skew exceeds tolerance — alarm raised". |
| Missing F-NG (replay attack with cert rotation) | A-NG19 added; F-NG11 added (P0-if-mTLS-bypass; design-resistant). |
| Missing F-NG (server-side proxy mid-fail) | A-NG20 added; F-NG10 added; Gherkin scenario "Server-side proxy fallback — proxying node fails mid-proxy". |

All 19 findings + 4 missing-failure-mode gaps resolved in the analyst-layer artifacts.

Final spec-layer counts:
- Ubiquitous language: 16 new terms (was 11)
- A-NG entries: 20 (was 11)
- I-NG entries: 14 (was 10) — split + 4 new
- Gherkin scenarios: ~30 (was 19)
- F-NG failure modes: 11 (was 7) — total catalog 45 modes (was 41)

Architect can pick up. The remaining open item is the in-process floor measurement before ADR-042's protocol shape commits.
