# Adversary Gate-1 Round 2 Findings — ADR-043 rev 2 + libfuse-swap plan

**Type**: Adversary → Architect (gate-1 round 2)
**Date**: 2026-05-09
**Reviewer**: adversary (architecture mode)
**Mode**: pre-acceptance review against `c5014a3` (ADR-043 rev 2) and `6fb88aa` (D6 amendment + libfuse-swap.md plan rename).
**Verdict**: **CHANGES REQUESTED — small scope.** 0 CRITICAL, 3 HIGH, 6 MEDIUM, 3 LOW. Most issues are about §D6's new "plan-gated" mechanism and gaps in `specs/implementation/libfuse-swap.md` that the previous gate-1 didn't see (the plan didn't exist then).

ADR-043 rev 2 is **structurally improved** over rev 1: scope correctly reduced to FFI policy, all rev-1 CRITICALs (F-C1/C2/C3) and most HIGHs dissolved with the ganesha removal, libfabric precedent argument cleanly preserved. The rev-1 findings still in flight (F-H2, F-H4, F-M1, F-M5, F-M8, F-M9) are addressed in the rev-2 text. **Rev 2 itself is acceptance-ready** modulo the small set of issues below; the larger concern is that the implementation plan, which §D6 now positions as the architectural-decision document for the libfuse swap, has gaps that previous review didn't cover.

## Summary

| Severity | Count |
|---|---|
| Critical | 0 |
| High     | 3 |
| Medium   | 6 |
| Low      | 3 |

---

## HIGH findings

### F2-H1: §D6 criteria are illustrative, not exhaustive — process escape hatch

**Severity**: High
**Category**: Correctness > Specification compliance, Robustness > Observability gaps
**Location**: ADR-043 §D6 ("e.g., a process-isolated daemon, a new auth shape, a cross-language schema, or a new ubiquitous-language term")

**Description**: The new §D6 says a per-binding ADR is required ONLY when the binding introduces architectural decisions "beyond the policy in this ADR — e.g., a process-isolated daemon (a new bounded-context boundary), a new auth shape, a cross-language schema, or a new ubiquitous-language term." The list is illustrative ("e.g."), not exhaustive. A future architect deciding whether to write a per-binding ADR has no sharp test.

Concrete examples that the illustrative list doesn't clearly cover:

- **License posture shift** — adopting a binding under a license that propagates to downstream wrappers (e.g., GPL-3 vs LGPL-2.1 vs MIT). License changes affect downstream distribution and packaging, which is architectural. Should this trigger an ADR?
- **FIPS evaluator scope expansion** — a binding that requires an additional FIPS module audit to keep the certification clean. Same question.
- **Build-system / packaging changes** — a binding that requires a new package on every supported distro, potentially breaking older Debian/Ubuntu/RHEL releases.
- **Network-stack-shape changes** — a binding that changes how kiseki listens or accepts connections (e.g., a binding that adds RDMA listeners on a new port).

The §D6 review-discipline note partially saves this: "The review verifies that the plan does not silently introduce architectural decisions (per the criteria above) that should have warranted a per-binding ADR after all." But "the criteria above" is exactly the illustrative list. Self-referential.

**Suggested resolution**: Replace "e.g." with an explicit checklist. Adopt a binding via plan-only when **none** of the following apply; otherwise a per-binding ADR is required:

1. The binding introduces a new bounded-context boundary (a new OS process, a new RPC service, or a new cross-language wire format kiseki maintains).
2. The binding's auth/authz model differs from kiseki's existing tenant identity propagation.
3. The binding introduces a new ubiquitous-language term (`specs/ubiquitous-language.md` gains a new entry).
4. The binding's license materially changes downstream distribution shape (LGPL ↔ permissive ↔ copyleft transitions; or wrapper LGPL exposure that didn't exist).
5. The binding's distribution shape requires new packaging steps on the GCP perf cluster, the dev environment, or downstream wrapper builds.
6. The binding adds a new failure mode (per `specs/failure-modes.md`) or a new invariant.
7. The binding's adoption changes any existing ADR's decision (i.e., requires another ADR to be revised).

Add this checklist to §D6, with a one-line rule: "If any answer is yes, write a per-binding ADR. The architect documents the answer to each before merging the plan."

This makes the criterion machine-checkable (almost) and removes the discretion that lets architectural decisions slip through under "I judged this not architectural enough."

---

### F2-H2: libfuse-swap.md acceptance criteria don't verify D1.1 security-posture data is filled in

**Severity**: High
**Category**: Security > Cryptographic correctness, Correctness > Specification compliance
**Location**: `specs/implementation/libfuse-swap.md` §"Acceptance criteria"; cross-reference to ADR-043 §D1.1

**Description**: ADR-043 §D1.1 (added rev-2 per gate-1 F-H2) requires every D2 positive-list entry to name (a) upstream's security-issue handling history, (b) most recent CVE and time-to-patch, (c) kiseki team's stated triage SLA. The libfuse row in §D2 is **the new entry** that triggered §D1.1; it is therefore the first acceptance test of the rule.

`libfuse-swap.md`'s acceptance criteria (§"Acceptance criteria", 6 bullet points) does not include verifying D1.1 is populated. The plan can be marked "done" with libfuse swapped in but no security-posture row in the ADR — exactly the failure §D1.1 was added to prevent.

**Evidence**: ADR-043 §D2 currently shows only build/version metadata for libfuse:

```
| `libfuse` 3.x | FUSE protocol dispatch | `kiseki-fuse-sys` ... | No | 3.10 (FUSE_SYNCFS support) | **New rev-2 addition.** ... |
```

The "Notes" cell does not include CVE history, last-patch lag, or kiseki triage SLA per D1.1.

**Suggested resolution**: Add to libfuse-swap.md §"Acceptance criteria": "(7) ADR-043 §D2 libfuse row is updated with D1.1 security-posture data: upstream CVE history (point to the libfuse advisories page or equivalent), most recent CVE and time-to-patch, kiseki triage SLA (default per D1.1: CRITICAL ≤ 7d, HIGH ≤ 30d, MEDIUM at next release)." This locks the plan's "done" state to the policy's stated requirement.

---

### F2-H3: libfuse-swap.md doesn't address FFI safety / cancellation-safety in the wrapper

**Severity**: High
**Category**: Robustness > Error handling quality, Security > Trust boundaries
**Location**: `specs/implementation/libfuse-swap.md` §"Crate layout" + §"Risk register"

**Description**: The plan describes the safe wrapper crate (`kiseki-fuse`) as exposing a `Filesystem`-shaped trait close to fuser's. It does not specify:

- **Cancellation safety**: what happens when a tokio task driving an FFI call is dropped mid-call? libfuse 3.x's session loop is a C-level thread (multi-thread) holding state per request; if a Rust async task driving the request is cancelled, the C-side reply slot may be reclaimed but the user's `Filesystem` impl future is dropped. Reply objects (`ReplyAttr`, `ReplyData`) typically must be consumed exactly once or libfuse leaks the request slot. Dropping a reply without consuming it is a known FUSE bug class.
- **Use-after-free**: if a `ReplyData` borrows from a buffer the kernel still holds, dropping the reply outside libfuse's expected order can free a buffer the C side still references.
- **Send + Sync**: the safe wrapper traits show `Send + Sync + 'static` — but libfuse's reply types are typically not safe to send across threads in unrestricted ways. The plan doesn't enforce that the trait surface matches libfuse's actual concurrency contract.

The risk register names "FFI safety bugs in `kiseki-fuse-sys` (use-after-free in a reply)" and assigns mitigation to "Adversary review on `kiseki-fuse` crate before merge — gate-2 audit at minimum." That's a reasonable plan but pushes the harder questions to a later review without specifying what the wrapper's cancellation contract IS. Better: state the contract in the plan, then audit against it.

**Suggested resolution**: Add to §"Crate layout" a sub-section "Safety contract" that states:

1. Reply types do NOT implement `Drop`-without-consume; if a Rust handler returns without consuming the reply, the wrapper records this as a logic error and either replies with `EIO` or leaks the slot (panic-in-debug, leak-in-release). Specify the choice.
2. Reply types are `!Send` or `Send`-but-bound-to-a-thread-token to prevent cross-thread dispatch with an unrelated session.
3. The session loop runs on a `tokio::task::spawn_blocking` thread (or equivalent libfuse-thread-aware bridge); Rust async tasks driving handlers see the reply object only as a wrapped consume-once token.
4. Cancellation: if the tokio task driving a handler is cancelled, the wrapper completes the libfuse reply with `EINTR` before the handler future is dropped. This requires a `Drop` impl on the consume-once token that does the EINTR fallback if not consumed.

Then the wrapper is auditable against this contract; the gate-2 audit has concrete attack targets.

---

## MEDIUM findings

### F2-M1: §D2 grandfathered entries (libfabric, libibverbs) lack D1.1 security-posture data

**Severity**: Medium
**Category**: Correctness > Implicit coupling, Specification compliance
**Location**: ADR-043 §D2 table + §D1.1

**Description**: §D1.1 (rev-2 addition) requires CVE history, time-to-patch, and triage SLA for every D2 entry. The libfabric and libibverbs rows say "Existing — pre-permitted by ADR-001" without that data. They're grandfathered in but the rule is universal as stated.

**Suggested resolution**: Either (a) add §D1.1 data for the existing rows in this ADR, or (b) state explicitly that pre-existing pre-ADR-043 entries are grandfathered with a deferred D1.1-data deadline (e.g., "next rev"). Option (a) is more honest and doesn't leave a future-rev queue that decays.

### F2-M2: §D6 review-discipline note doesn't specify the milestone

**Severity**: Medium
**Category**: Robustness > Failure cascades, Operational
**Location**: ADR-043 §D6 ("the implementation plan IS reviewed by adversary gate-1 ... BEFORE implementer phase 0")

**Description**: "Before implementer phase 0" is a sequencing constraint, but the plan can be edited after gate-1 review. If the plan is amended (new phases, scope creep) after review but before implementation completion, who re-reviews? The note doesn't specify a "stale plan triggers re-review" rule.

**Suggested resolution**: Add: "Material amendments to a reviewed plan (new phases, new dependencies, scope expansion, removal of acceptance criteria) trigger a gate-1 round 2 on the amended sections only. The architect MUST NOT silently widen scope post-gate-1 without re-review."

### F2-M3: libfuse-swap.md §"Phase 0" doesn't audit GCP perf-cluster build paths

**Severity**: Medium
**Category**: Robustness > Failure cascades, Operational
**Location**: `specs/implementation/libfuse-swap.md` §"Phase 0 — pre-work"

**Description**: Phase 0 says CI runners need `libfuse3-dev`. It doesn't mention the GCP perf cluster setup scripts (`infra/gcp/benchmarks/`, `.gcp-build/build.sh`) that build kiseki binaries for the production-shape perf runs. If those build paths don't install `libfuse3-dev`, the next perf-cluster run breaks.

**Suggested resolution**: Add to Phase 0: "(3) Audit `.gcp-build/build.sh` and `infra/gcp/` setup scripts for missing `libfuse3-dev` / `fuse3-devel` install. The GCP cluster builds the kiseki-client binary with `--features fuse,remote-http,native`; the libfuse swap means the build now requires libfuse3 headers." The audit should produce a small PR to those scripts before Phase 1.

### F2-M4: libfuse-swap.md doesn't operationalize §D5's go/no-go review

**Severity**: Medium
**Category**: Correctness > Implicit coupling, Operational
**Location**: `specs/implementation/libfuse-swap.md` §"Acceptance criteria"; cross-reference to ADR-043 §D5

**Description**: ADR-043 §D5 commits each binding to a "go / no-go review at a specific milestone (e.g., 6 months of cluster operation post-merge)." The libfuse-swap plan doesn't pick a date or define what evidence the review uses (perf-cluster numbers? CVE incident count? FUSE BDD flake rate?).

**Suggested resolution**: Add to libfuse-swap.md a §"Go/no-go review" section: "Six months after `fuser` is removed from `Cargo.lock` (final D6 step), the architect runs a review against: (a) Tier_1 perf-cluster numbers stayed within ±10% of pre-swap baseline; (b) zero unpatched libfuse CVEs CRITICAL/HIGH in the kiseki-pinned version range during the period; (c) FUSE BDD flake rate did not regress. If any criterion fails, mark the libfuse row Rejected and revert per §D5."

### F2-M5: libfuse-swap.md doesn't specify what triggers a roll-back mid-stream

**Severity**: Medium
**Category**: Robustness > Failure cascades
**Location**: `specs/implementation/libfuse-swap.md` (no §"Rollback")

**Description**: The plan estimates 4-6 days for the swap. If something goes badly wrong on day 4 (e.g., the BDD suite has a 30% flake regression, or a use-after-free bug surfaces), what's the rollback? §D5 says "pre-existing pure-Rust paths remain in tree until the plan delivers parity." That covers the *not yet merged to main* case. It doesn't cover "we merged Phase 2 but Phase 4 reveals an integration regression."

**Suggested resolution**: Add §"Rollback procedure" stating: "Each phase is a standalone PR. Phases 1-2 (new crates + port `fuse_daemon.rs`) merge gated on local-only Tier-1 + a smoke test of the swap. Phase 3-4 (test files + BDD) gate the *removal* of `fuser` from production code paths — until Phase 6 lands, both `fuser` and `kiseki-fuse` are present in the workspace, and the `fuse` feature flag selects which one `fuse_daemon.rs` uses (via cfg). If Phase 4 regresses, the cfg flips back to fuser; no commits revert."

### F2-M6: §D2 libfuse row notes "FUSE_SYNCFS support" but doesn't clarify kernel vs userspace dependency

**Severity**: Medium
**Category**: Correctness > Implicit coupling, Specification compliance
**Location**: ADR-043 §D2 (libfuse row "Min version 3.10 (FUSE_SYNCFS support)")

**Description**: FUSE_SYNCFS opcode 50 was added to the FUSE protocol in **kernel ≥ 5.1** (2019). libfuse 3.x exposes the `syncfs` callback from approximately 3.0 onward (the protocol-version negotiation is what gates kernel-side dispatch). The §D2 note conflates "min libfuse version" with "min kernel version," which can mislead operators who run kiseki on an older kernel with newer libfuse.

**Suggested resolution**: Update the §D2 row's notes: "Min libfuse 3.10 (Debian/Ubuntu LTS shipped) + kernel ≥ 5.4 (covers FUSE_SYNCFS opcode 50, IORING-async hints, etc.). Operator docs document the kernel floor."

---

## LOW findings

### F2-L1: §D5 reversibility doesn't name who triggers the 6-month review

**Severity**: Low
**Category**: Operational
**Location**: ADR-043 §D5

**Description**: "Each binding's adoption is governed by an implementation plan ... commits to a 'go / no-go' review at a specific milestone (e.g., 6 months of cluster operation post-merge)." Who calendar-tracks this? Without naming an owner, the review may not happen.

**Suggested resolution**: "The architect (currently the workflow owner per `.claude/CLAUDE.md`) tracks the date in `specs/architecture/adr/043-system-library-ffi.md` itself, in a `## Review schedule` section appended at acceptance, with concrete dates per binding."

### F2-L2: Industry comparison from rev 1 was deleted; libfuse-vs-fuser-rs comparison no longer in the ADR

**Severity**: Low
**Category**: Correctness > Specification compliance
**Location**: ADR-043 §Rationale (rev-2 trimmed the comparison table)

**Description**: Rev 1 had an industry comparison table (VAST/Ceph/Lustre/JuiceFS/etc.) that grounded the "we're hand-rolling unusually much" claim. Rev 2 removed it (since the rev-1 conclusion doesn't apply once ganesha is dropped). But the libfuse-vs-fuser-rs decision in §"Why libfuse and not 'fix fuser-rs upstream'" doesn't reference the production base of libfuse use the rev-1 table established. A reader landing on rev-2 fresh has less context for why libfuse is the obvious choice over fuser-PR-only.

**Suggested resolution**: Add a one-paragraph note in §"Why libfuse and not 'fix fuser-rs upstream'" with concrete examples: "libfuse 3.x is the reference impl ... used in production by sshfs (millions of deployments), juicefs, gcsfuse (Google Cloud Storage FUSE), dfuse (DAOS), ceph-fuse, gocryptfs."

### F2-L3: libfuse-swap.md §"macOS posture" decides retirement; could merit a §D6 architectural-decision flag

**Severity**: Low
**Category**: Correctness > Implicit coupling
**Location**: `specs/implementation/libfuse-swap.md` §"macOS posture"

**Description**: Retiring macOS FUSE support is a platform-scope reduction. Per the strict reading of the (revised) §D6 checklist (F2-H1's suggested fix), this borderlines on "distribution shape change" / "supported-platform retirement," which arguably warrants per-binding ADR scrutiny. The plan section is well-reasoned but slim (5 paragraphs).

**Suggested resolution**: Either upgrade `macOS posture` to a top-level §"Platform-scope retirement" with explicit BEFORE/AFTER list of supported targets, or punt the decision to the gate-1 round 3 review of this plan (i.e., re-flag macOS retirement when this plan is reviewed in detail).

---

## Cross-cutting

### CC2-1: The new §D6 review-discipline says "findings live in `specs/findings/` keyed on plan filename" — but the existing convention is "keyed on artifact + date." Plan filename without date is ambiguous if a plan is amended over time.

**Suggested resolution**: Adopt the existing convention: `specs/findings/YYYY-MM-DD-adv-gate1-<plan-base-name>-findings.md`. This file would be `2026-05-09-adv-gate1-round2-adr043-findings.md` (current naming) but for a plan-targeted review of `libfuse-swap.md` the filename would be `2026-05-09-adv-gate1-libfuse-swap-findings.md`.

---

## Verdict

**CHANGES REQUESTED — small scope.** ADR-043 rev 2 is the right shape; the issues are narrow and fixable in one architect-only round.

What blocks acceptance:
- F2-H1 (§D6 illustrative criteria → exhaustive checklist) — 7-line addition to §D6.
- F2-H2 (D1.1 acceptance check in libfuse-swap.md) — one line in plan acceptance criteria.
- F2-H3 (FFI safety contract in libfuse-swap.md) — one new sub-section in plan §"Crate layout."

What can be deferred to the libfuse-swap plan's own gate-1 round (when it's the active reviewed artifact):
- All MEDIUMs except F2-M1 (which is an ADR §D2 issue, not a plan issue).
- All LOWs.

Estimated amendment size: ~30-40 lines of additions, split across ADR (F2-H1, F2-M1, F2-M2, F2-M6, F2-L1, F2-L2) and plan (F2-H2, F2-H3, F2-M3, F2-M4, F2-M5, F2-L3). One round, no second analyst pass needed.

After amendment: ADR-043 rev 2 moves to **Acceptance pending only on Open item B** (FIPS evaluator written reference per the original gate-1 F-H4). The libfuse-swap plan is then ready for its OWN gate-1 round (the one §D6 now requires before implementer phase 0) — that round attacks the plan's specific concrete decisions (binding-crate version pins, trait-surface choices, perf-floor commits, FFI-safety contract). The current findings document the macro-policy issues; the plan-specific gate-1 will do the micro-implementation attack.
