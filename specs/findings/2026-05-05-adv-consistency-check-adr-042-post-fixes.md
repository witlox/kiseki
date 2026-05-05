# Adversary Consistency Check — ADR-042 post-gate-0-fixes

**Type**: Adversary → Architect (gate-0-amendment verification)
**Date**: 2026-05-05
**Reviewer**: adversary (architecture mode)
**Trigger**: User directed "if ok pass to architect" after the analyst resolved all 19 gate-0 findings + 4 missing-failure-mode gaps in place.
**Verdict**: **PASS WITH MINOR FIXES APPLIED** — 2 issues found, both fixed in place during this consistency review. Architect can pick up.

## Method

Re-read every artifact touched by the gate-0 resolutions and look for:
1. Old-text vs new-text contradictions (did the amendments leave stale references?)
2. Cross-reference integrity (do I-NG entries reference real invariants? do A-NG entries align with existing assumptions?)
3. Gherkin-to-invariant mapping (does every new I-NG have a scenario? does every scenario test the invariant it claims?)
4. Severity calibration (do F-NG severities match the new failure semantics?)
5. New gaps introduced by the resolutions

## Findings

### F-CK1 (Low, FIXED): I-NG7 referenced wrong invariant for stream cap

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: `specs/invariants.md` I-NG7

**Description**: I-NG7 listed the proto-handler-boundary rejections as "I-NG1 SAN/payload mismatch, I-NG6c missing dual-flag, **I-NG14 stream cap**". The stream cap is I-NG11 (added by gate-0 F-H6 resolution), not I-NG14 (drain interaction). Editing oversight from the F-M1 resolution.

**Fix applied**: I-NG7 now reads "I-NG1 SAN/payload mismatch, I-NG6c missing dual-flag, I-NG11 stream cap exceeded, I-NG12 fenced write, I-NG14 lease-against-draining-node". Expanded the list to be exhaustive about which proto-handler-boundary rejections fire the security-failure audit hook.

### F-CK2 (Medium, FIXED): "DEK-fetch ticket" was used in I-NG6a but not first-class in ubiquitous language

**Severity**: Medium
**Category**: Correctness > Semantic drift
**Location**: `specs/invariants.md` I-NG6a; `specs/ubiquitous-language.md` (missing entry)

**Description**: I-NG6a (resolution to F-H5) introduces a new mechanism — an opaque server-signed ticket that commits to the at-Read-time `crypto_boundary` mode and is passed to the keymanager during DEK fetch. The mechanism is referenced in I-NG6a's text and in the gate-0 finding resolution table, but does not have a first-class ubiquitous-language entry. Future architect / implementer / adversary work on the ticket would have no canonical definition to anchor against, risking semantic drift (one implementation makes the ticket cleartext, another HMACs it, etc.).

**Fix applied**: New ubiquitous-language entry "DEK-fetch ticket" added to the Native gateway section, specifying: (a) what the ticket commits to — `(tenant_id, namespace_id, composition_id, chunk_id, namespace crypto_boundary mode at Read time)`; (b) signing mechanism — HMAC under the system DEK; (c) why it resolves the F-H5 race — captured-ticket replay across compositions/chunks is prevented by the chunk-id binding, and flag-flip races are resolved at DEK-fetch time by the keymanager validating the at-Read-time mode against the namespace's current policy.

## Strategic observation (not a finding)

The asymmetric performance target in A-NG11 (post in-process floor measurement) reveals a **strategic gap**: the user's target is "close to WekaFS" (≈150 k op/s per client). In-process PUT floor is 20 k op/s — an order of magnitude below WekaFS-class write throughput, even before any wire protocol. **ADR-042's 14 k op/s native PUT target is the realistic protocol-design target on this gateway-internal hot path; closing the gap to 150 k op/s requires gateway-internal-write-path work that is not in ADR-042's scope.**

A-NG11 captures this faithfully ("Higher PUT throughput requires a separate gateway-internal-write-path ADR"), and the analyst handoff escalation calls it out as a future scoping decision. This is not a finding against the analyst output; it is a scope-of-work observation the architect should weigh when sequencing the next ADR.

## Coverage validation

Walked through every I-NG / A-NG / F-NG and matched against Gherkin scenarios. All present:

| Invariant | Scenario | Status |
|---|---|---|
| I-NG1 (cert-SAN tenant binding) | "Native PUT — cert SAN matches", "cert SAN does NOT match", + 6-row Scenario Outline for canonicalization near-misses | ✓ |
| I-NG2 (commit-on-close) | "object PUT — small payload", "object PUT — streaming, commit-on-close", "object PUT — stream interrupted" | ✓ |
| I-NG3 (POSIX fsync visibility, A-NG16 read-leader-routing) | "POSIX write — partial-visible-on-fsync" | ✓ |
| I-NG4 (cross-shard EXDEV) | "POSIX rename within shard — atomic", "rename across shards — EXDEV" | ✓ |
| I-NG5 (idempotency dedup) | "Native object PUT — retry with same idempotency_key" | ✓ |
| I-NG6a (object verbs, DEK-fetch ticket) | "Native Read — server-decrypt mode (default)", "client-decrypt mode (trusted compute pool)" | ✓ |
| I-NG6b (POSIX verbs, handle token) | covered by I-NG3 scenario + handle-token mechanism in A-NG6 | ✓ (mechanism, not failure mode) |
| I-NG6c (TrustedCompute requires best-effort shred) | "TrustedCompute requested without best-effort shred — rejected", "with explicit best-effort shred — accepted" | ✓ |
| I-NG7 (audit on every op + proto-boundary security-failure) | "Native data op — audit event uses cert SAN as principal" + implicit in F-NG1 scenario | ✓ |
| I-NG8 (hybrid leader routing) | "Native client receives NotLeader and refreshes topology", "Native client transparently uses server-side proxy fallback", "Server-side proxy fallback — proxying node fails mid-proxy" | ✓ |
| I-NG9 (streaming threshold) | "Native object PUT — small payload", "streaming payload, commit-on-close", "above per-stream cap goes multipart" | ✓ |
| I-NG10 (lease-based RMW) | "POSIX lease-based RMW — exclusive write", "lease holder dies, lease expires" | ✓ |
| I-NG11 (concurrent-stream cap) | "Per-tenant concurrent stream cap enforced at proto boundary" | ✓ |
| I-NG12 (fencing token) | "POSIX lease — partition-heal split-brain rejected by fencing", "Fencing token recorded in audit event" | ✓ |
| I-NG13 (topology version) | "Topology cache refreshed on topology_version mismatch" | ✓ |
| I-NG14 (drain interaction) | "Drain waits for outstanding leases", "New AcquireLease against a draining node — rejected" | ✓ |

Failure modes:
| F-NG | Source | Status |
|---|---|---|
| F-NG1 (cert/payload mismatch) | F-H3 + original | ✓ + Scenario Outline 6 cases |
| F-NG2 (stream interrupted before CommitStream) | original | ✓ |
| F-NG3 (leader change mid-stream) | original | ✓ |
| F-NG4 (lease holder crash) | F-H4 amendment via I-NG12 | ✓ |
| F-NG5 (DEK fetch failure) | original | ✓ |
| F-NG6 (idempotency cross-tenant — design-resistant) | original | ✓ |
| F-NG7 (heartbeat starvation) | A-NG9 risk acknowledgment | ✓ |
| F-NG8 (cert revocation mid-session) | gap-fill 2026-05-05 | ✓ + scenario |
| F-NG9 (clock skew) | gap-fill 2026-05-05 | ✓ + 2 scenarios |
| F-NG10 (proxy mid-fail) | gap-fill 2026-05-05 | ✓ + scenario |
| F-NG11 (replay attack with cert rotation — design-resistant) | gap-fill 2026-05-05 | ✓ |

## Cross-reference integrity (sample audit)

Spot-checked 10 cross-references:

- I-NG1 → "Canonical SAN URI form" → ubiquitous-language entry exists ✓
- I-NG2 → "preserves I-L5" → I-L5 in invariants.md is the composition-not-visible-until-chunks-durable invariant ✓
- I-NG3 → "per A-NG16" → A-NG16 in assumptions.md is the POSIX read-after-fsync routing assumption ✓
- I-NG4 → "preserves I-L8" → I-L8 in invariants.md is the cross-shard EXDEV invariant ✓
- I-NG5 → "1..=64 bytes ... longer rejected at the proto-handler per I-NG1" → I-NG1 includes the length validation ✓
- I-NG6a → "DEK-fetch ticket" → ubiquitous-language entry now exists (added during this consistency check) ✓
- I-NG7 → "I-NG11 stream cap exceeded" → I-NG11 is the concurrent-stream cap (corrected during this consistency check) ✓
- I-NG10 → "see I-NG12" → I-NG12 is the fencing token invariant ✓
- I-NG13 → "the cache TTL (I-NG8) remains a safety-net" → I-NG8 specifies the TTL ✓
- I-NG14 → "cross-context with ADR-035" → ADR-035 exists in `specs/architecture/adr/035-node-lifecycle-drain.md` ✓

No broken references after the two fixes above.

## Final spec-layer counts (post consistency check)

- Ubiquitous language: 17 native-gateway terms (16 from gate-0 + DEK-fetch ticket added during consistency check)
- A-NG entries: 20 (8 originals + A-NG7/A-NG10/A-NG11 amended + 9 added via gate-0 = 11 net new + 8 amendments)
- I-NG entries: 14 unique IDs (I-NG6 split into 6a/6b/6c + I-NG11/12/13/14 added)
- Gherkin scenarios: 33 (6 of those are Scenario Outline rows; 27 distinct scenario specs)
- F-NG failure modes: 11; total catalog 45 modes (was 41 pre-gate-0)

## Verdict

**PASS WITH MINOR FIXES APPLIED.** All gate-0 resolutions are internally consistent after the F-CK1 (typo) and F-CK2 (missing term) fixes. The ubiquitous-language additions, invariants, assumptions, Gherkin coverage, and failure-mode entries are mutually coherent. The strategic observation about WekaFS-class write throughput is captured honestly in A-NG11; ADR-042's scope (PUT target ≥14 k op/s) is realistic given the measured floor.

**Recommend architect proceeds to draft ADR-042.** The 7 architect-level open questions in `specs/escalations/2026-05-05-analyst-handoff-adr-042-native-gateway.md` (lease heartbeat cadence, discovery RPC contract, mid-stream resume token, multipart shape, mTLS SAN-role interceptor, federation behavior, implementation primitive choices) are correctly architect-scoped after the analyst-layer fixes.
