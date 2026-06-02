# Adversary gate-1 — ADR-047 (decoupled write ack / quorum-durable intent + async ordering)

**Reviewer**: Adversary role (architecture mode)
**Date**: 2026-05-29
**Artifact**: `specs/architecture/adr/047-decoupled-write-ack.md`
**Verdict**: **CHANGES REQUESTED** — the model is *sound in principle* (the no-loss floor holds; the four obligations are **satisfiable**), but **3 Critical + 1 High** block implementation. The relaxation merely **moves** the synchronous barrier (Raft round → stability watermark) and **splits durability across two structures** (intent store + Raft log); both moves introduce hazards the ADR under-specifies.
**Counts**: 3C + 1H + 3M.

Verified the no-loss claim: an intent acked on `min_acks`=2 of RF-3 survives any single failure, and a new leader's 2-of-3 quorum overlaps the intent's 2-of-3 quorum by ≥1 — so **no acked write is lost** as long as recovery actually gathers it. That floor is real. The holes are in *ordering liveness*, *cross-structure crash consistency*, and *POSIX visibility*.

---

## Part A — explicit check of the four obligations

### O1 — HLC perspective-seq → deterministic order, no split-brain
**CONDITIONAL PASS.** No split-brain: two ingress nodes stamping the same name get distinct seqs (node_id breaks ties) and the leader applies in seq order — one deterministic LWW winner, no divergence. ✓
But "deterministic" ≠ "correct": **§1 gives no skew bound** (see F-5) so a clock-ahead node silently wins every race; and determinism-without-rollback holds **only if the stability watermark is live** (see F-1). O1 is safe but its *correctness* rides on F-1 + F-5.

### O2 — quorum-durable intent survives leader election
**CONDITIONAL PASS.** Steady-state 1-failure: holds (quorum overlap ≥1). ✓
Gaps: (a) the **intent-recovery gather (§4) is a quorum read of a non-Raft structure during election** — its interaction with openraft's own election (which only knows the Raft log) is unspecified; the new leader must complete intent-recovery *before* resuming apply, and the ADR doesn't sequence this against openraft becoming leader. (b) **Election concurrent with a membership change** (ADR-035 drain) can break the static quorum-overlap argument — the intent's quorum and the new config's quorum may not overlap. Must be specified.

### O3 — crash/retry idempotency
**CONDITIONAL PASS.** Keying on the **client `idempotency_key`** (§5), not the server perspective-seq, is the right call. ✓
But the leaderless intent-write dedup is **not atomic across the quorum**: two concurrent ingress of the same key on different nodes each fan an intent to overlapping quorums → apply-time dedup (§5) drops the duplicate *delta*, but **both ingress already ran the EC fan-out** → relies entirely on content-addressed chunk dedup (same `chunk_id`) to avoid orphans. The ADR must state that dependency, and that the apply-time dedup index is **durable and replay-safe** (else F-2 re-applies a deduped key).

### O4 — `min_acks` ≥ 2 tolerates ≥1 failure before ack
**PASS, with a required disclosure.** 1-failure tolerance and the 2-simultaneous-loss exposure are **identical to the current Raft majority commit** — not worse. ✓
Required: the ADR states the *data* floor inherits the I-L5 page-cache/flush window, but is **silent on the metadata intent's durability mode**. If the intent is group-committed to page cache (likely, for speed), a correlated power loss within the flush window loses *acked* intents → silent loss. State whether the intent is `fsync`-on-ack or page-cache, and the resulting correlated-loss window (mirror the I-L5 disclosure).

**Summary:** all four are *satisfiable*, none is *unconditionally satisfied as written*. Each needs a named mechanism. That is the gate's core message: the obligations are the easy part; the mechanisms below are the hard part.

---

## Part B — findings the four obligations do not cover

## Finding: Stability watermark requires ALL of the quorum → one laggard stalls all applies
Severity: **Critical**
Category: Robustness > resource exhaustion / Correctness > failure cascades
Location: ADR-047 §3 step 1
Description: W advances only when **"every replica in the quorum has reported"** its low-water-mark. A single slow/GC-paused/silent replica never advances its gossip → W freezes → **no intents apply** → visibility lag unbounded → intent store grows unbounded (F-6) → POSIX reads hit the pending path forever. This re-introduces a synchronous all-replicas barrier — the exact class of stall we removed from the ack path, relocated to the apply path.
Evidence: §3 "every replica in the quorum"; no timeout, no exclusion, no force-advance.
Suggested resolution: advance W on a **majority** low-water-mark (mirror Raft commit-on-majority), exclude a replica lagging > T from the watermark set (it catches up via normal replication), and bound the apply lag with backpressure.

## Finding: Intent store ↔ Raft log crash consistency undefined → double-apply or lost-apply
Severity: **Critical**
Category: Correctness > implicit/temporal coupling
Location: ADR-047 §3 step 2, §4
Description: Apply = {order intents → append to Raft log → commit+apply → prune intent store}. Intent store and Raft log are **two durable structures with no defined atomic boundary**. Crash *after* Raft commit but *before* prune → recovery re-gathers the still-pending intent and re-incorporates it → **double-apply** (duplicate delta) unless the §5 dedup index is durable, replicated, and consulted on replay — which the ADR does not require. Crash with prune-before-commit-durable → **lost apply**.
Evidence: §3/§4 describe the happy path; no idempotent re-incorporation / barrier spec.
Suggested resolution: make incorporation idempotent — the Raft entry records the perspective-seqs it incorporated; on recovery, intents whose seq ≤ the log's max-incorporated-seq are dropped, not re-applied. Define prune as derivable from the log (prune is an optimization, never a correctness dependency).

## Finding: POSIX close-to-open pending-intent read must be a quorum read and is unsafe during recovery
Severity: **Critical**
Category: Correctness > specification compliance (ADR-013 close-to-open)
Location: ADR-047 §6 (POSIX bullet)
Description: §6 says a POSIX read consults "the per-shard intent store." A **single-node** consult misses an intent held on the other `min_acks` nodes (intent on {A,B}, read served by C) → a read-after-close that returns stale/missing → **close-to-open violation** (ADR-013). To be correct the POSIX read-miss must **quorum-read** the intent store (latency + load on every miss), and **during intent-recovery** (§4) the store is mid-gather → a window where even a quorum read sees a partial set. For an object/S3 surface this is fine (eventual, I-CS2); for a POSIX filesystem it is a correctness break.
Evidence: §6 does not state the read's quorum or its behavior during recovery.
Suggested resolution: POSIX read-miss does a quorum read of the intent store keyed by name; reads are fenced during intent-recovery (serve from the recovered set only after the gather completes). Or: POSIX namespaces apply synchronously (opt back into the strict path) — accept that close-to-open and async-apply are in tension and POSIX may not get the relaxation.

## Finding: Intent store is a new durable, replicated format — mixed-version rolling-upgrade hazard
Severity: **High**
Category: Correctness > failure cascades
Location: ADR-047 §2, Rollout
Description: Same class as ADR-046 gate-1 C1. The intent is leaderless-written to a quorum that, mid-upgrade, may contain a node on old code that cannot decode it → the write diverges or fails. The Rollout names a two-release migration but does not make it a **blocking mechanism** with a defined failure mode for a non-understanding replica.
Suggested resolution: decode-capable everywhere one release before any node *writes* intents; capability gate (cluster-min-version, committed via the control-plane apply path) before any node *emits*; specify what happens when a non-advertising node joins (fall back to synchronous path).

## Finding: HLC skew bound unquantified → wrong LWW winner
Severity: Medium
Category: Correctness > edge cases
Location: ADR-047 §1
Description: "bound skew" via HLC merge is stated but not quantified; no clamp/reject for a node with gross skew (NTP down). A node N seconds ahead wins every same-name race and silently shadows correct concurrent writes. Deterministic, not lossy, but *wrong*.
Suggested resolution: max-skew bound; reject or clamp intents whose physical_ms exceeds local HLC by > bound; alert.

## Finding: Intent-store unbounded growth under apply lag
Severity: Medium
Category: Robustness > resource exhaustion
Location: ADR-047 §3, Risks #6
Description: Coupled to F-1. With apply lag (or a stalled W), pending intents accumulate in durable storage with no cap. The ack path gains a hidden blocker (it must store the intent) that becomes a hard blocker (reject) only when bounded.
Suggested resolution: cap pending bytes/count per shard; backpressure (slow/reject ack) above the cap; surface `intent_pending_bytes`.

## Finding: I-NG2 / I-NG16 commit-on-close atomicity not restated for async apply
Severity: Medium
Category: Correctness > semantic drift
Location: ADR-047 §6, Consequences
Description: I-NG2 today: visible after CommitStream `Ok`. Under §2, `Ok` = quorum-durable intent (no loss) but visibility = post-apply (bounded-stale). The whole-delta intent is atomic so all-or-nothing *visibility* survives, but the invariant text and the multipart partial-failure path (I-NG16 orphan scrub of chunks staged before the delta intent is acked) must be restated.
Suggested resolution: rev I-NG2/I-NG16: `Ok` ⇒ durable; visibility ⇒ post-apply within I-CS2; partial multipart (chunks durable, delta intent not acked) → orphan scrub, unchanged.

---

## Recommendation
The decoupling is the right direction and the **no-loss floor is verified**. But as written the ADR trades the synchronous Raft round for (a) a synchronous all-replicas watermark (F-1), (b) an undefined two-structure crash boundary (F-2), and (c) a POSIX visibility break (F-3). **Resolve F-1/F-2/F-3 (Critical) + F-4 (High) in an ADR rev-2 before any code.** F-3 in particular may force a decision: **POSIX/NFS namespaces may have to stay on the synchronous-apply path** (close-to-open vs async-apply is a genuine tension), while S3/object/native get the relaxation — which, notably, is close to a per-*surface* (not per-namespace) split, and is fine because it's driven by the protocol's own consistency contract, not an arbitrary CP/AP knob.
