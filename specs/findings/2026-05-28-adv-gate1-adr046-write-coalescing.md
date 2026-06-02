# Adversary gate-1 — ADR-046 (write coalescing / batched Raft commit)

**Reviewer**: Adversary role (architecture mode)
**Date**: 2026-05-28
**Artifact**: `specs/architecture/adr/046-write-coalescing-batched-commit.md`
**Verdict**: **CHANGES REQUESTED** — 1 Critical + 3 High block implementation. Resolve in an ADR rev before any code.
**Counts**: 1C + 3H + 3M + 2L.

Verified against `kiseki-log/src/raft/state_machine.rs` (`apply_command`, `append_delta_inner`), `raft_store.rs` (`LogCommand`), and the gateway write path. The seq model is sound for batching (`self.tip` is a per-delta counter, *not* `log_index`, so N deltas/entry get N distinct seqs — last_applied/hydrator gap-detection survive). The holes are elsewhere.

---

## Finding: Mixed-version cluster cannot decode `BatchChunkAndDelta` — durable-log format break
Severity: **Critical**
Category: Correctness > failure cascades / robustness
Location: ADR-046 §1 + §Rollout; `kiseki-log/src/raft_store.rs::LogCommand` (serde/postcard, written into the durable Raft log)
Description: `LogCommand` is `Serialize/Deserialize` and its instances are persisted in the Raft log and shipped in `AppendEntries`. Adding a new variant is a **durable log-format change**. During a rolling upgrade, a new leader that commits a `BatchChunkAndDelta` entry sends it to followers still on old code; their `LogCommand` deserialize hits an unknown variant and **fails** — the follower cannot apply the entry, diverging the state machine (or panicking the apply task). This is a consensus-safety/availability hazard, not a feature flag.
Evidence: `apply_command` matches exhaustively on `LogCommand`; postcard enum decode of an unknown discriminant errors. ADR's mitigation — "gate on a cluster-min-version check or a control-plane feature flag, never emit to a cluster with an older replica" — names the intent but **no mechanism exists**: openraft has no command-version negotiation, and the leader has no built-in way to know every replica understands the variant.
Suggested resolution (advisory): a concrete capability gate. E.g. (a) a cluster-wide `WriteCoalesceEnabled` flag committed via the existing `SetShardConfig`/control-plane apply path, set ON **only after** every node has reported (via heartbeat/EXCHANGE or the control plane) that it runs a binary that understands the variant; the leader emits batches only when the committed flag is ON; OR (b) ship the *decoder* for `BatchChunkAndDelta` one release before any leader *emits* it (decode-capable everywhere, emit gated by config) — the standard two-release log-format migration. Either way the ADR must specify the gate, the failure mode if a non-understanding node joins, and the snapshot-format implications (a snapshot containing batched state restored on an old replica).

## Finding: Batch items share one HLC (`physical_ms = log_index`, `logical = 0`) — intra-batch causality is ambiguous
Severity: **High**
Category: Correctness > semantic drift / concurrency
Location: ADR-046 §2; `state_machine.rs::append_delta_inner:262-272`
Description: The delta timestamp is derived deterministically from `log_index` (`hlc.physical_ms = log_index`, `logical = 0`, `wall.millis = log_index`). One log entry == one `log_index`. A batch of N items applied under one entry therefore stamps **all N deltas with an identical HLC** (same physical_ms, logical 0). Any HLC-ordered consumer (conflict resolution, the hydrator's per-key apply order, cross-shard merges) cannot order two same-key deltas in the same batch. The single-write path never hit this because each delta had its own `log_index`.
Evidence: `append_delta_inner(..., log_index)` sets `physical_ms: log_index, logical: 0` unconditionally. ADR-046 §2 says "assigns each item a delta-seq" but is silent on the HLC.
Suggested resolution: the batch apply must pass an incrementing `logical` (= item index within the entry) to `append_delta_inner`, so the N deltas are `(physical_ms=log_index, logical=0..N-1)` — distinct, deterministic, monotonic. ADR must state this and that same-key items in a batch are ordered by `logical`.

## Finding: ADR places idempotency in the apply layer, but no apply-layer idempotency exists today
Severity: **High**
Category: Correctness > implicit coupling / semantic drift
Location: ADR-046 §1 (`+ idempotency_key`), §2 ("per-item idempotency during apply"), §Risks #3
Description: `LogCommand::ChunkAndDelta` has **no `idempotency_key` field**, and `apply_command`/`append_delta_inner` perform **no idempotency check** — the apply unconditionally appends. Idempotency today is enforced **gateway-side, pre-Raft** (`WriteRequest::idempotency_key`). ADR-046 proposes carrying `idempotency_key` into the log command and deduping "during apply", which (a) is a brand-new mechanism, not an extension of existing behavior, (b) would put idempotency state into the replicated state machine (new index, new memory, new snapshot content), and (c) changes the durability/dedup contract — all unjustified by the perf goal.
Evidence: the `LogCommand::ChunkAndDelta` field list (raft_store.rs:52-70) has no idempotency_key; `apply_command` has no dedup branch.
Suggested resolution: keep idempotency where it is — **gateway/coalescing-queue side, before the item enters a batch**. The queue dedups by `idempotency_key` (and resolves the duplicate's waiter with the in-flight/first result) *before* submitting the batch. Drop the apply-layer idempotency from the ADR; re-spec §2/§Risks #3 around queue-side dedup.

## Finding: `MAX_BATCH` is a count cap only — no batch byte-size bound
Severity: **High**
Category: Robustness > resource exhaustion
Location: ADR-046 §3 (`MAX_BATCH` items / `FLUSH_INTERVAL`)
Description: Each item carries a delta payload (and, for small objects, inline data up to the inline threshold). N items coalesced into one Raft entry produce one `AppendEntries` message and one log record whose size is ~ΣN payloads. A count-only cap lets a batch of large-payload items exceed the transport / log max message size, **failing the whole AppendEntries round** (and with it every writer in the batch). openraft's `max_payload_entries: 300` is an entry *count*, not a byte budget, and the fabric enforces a per-message cap.
Evidence: ADR §3 specifies only `MAX_BATCH` (count) and `FLUSH_INTERVAL`. Per-PUT inline payloads + 64 KiB small-object floor make ΣN unbounded for count-only batching.
Suggested resolution: add `MAX_BATCH_BYTES` (flush when accumulated payload bytes reach a fraction of the transport max-message, default well under it), flushing on whichever of count/bytes/interval trips first. An item larger than the byte budget is submitted as a singleton.

## Finding: Per-shard flush task is a new SPOF; oneshot/waiter lifecycle unspecified
Severity: Medium
Category: Robustness > failure cascades / error handling
Location: ADR-046 §3
Description: One flush task per shard sits on the critical path of every write to that shard. The ADR doesn't specify: behavior if the task panics (queue wedges, all writers hang), per-writer timeout, what happens to the oneshot when the client/ingress drops (leaked sender), or backpressure semantics beyond "bounded queue."
Suggested resolution: spec a per-item submit timeout (writer gives up + the queue tolerates a dropped receiver), a supervised/restartable flush task, and the bounded-queue overflow behavior (reject with a retryable error vs block).

## Finding: Commit-failure rollback of N pre-Raft local composition creates + forwarded-item fan-out
Severity: Medium
Category: Correctness > failure cascades
Location: ADR-046 §2/§5/§Risks #4
Description: The single-write path creates the composition locally pre-Raft and rolls it back on commit failure. For a batch, a commit failure must roll back **all N** local creates — and for #114-forwarded items the local create lives on the *ingress* node, so the failure must fan back to N (possibly different) ingress nodes, each rolling back its own. The ADR asserts "same window as today" without detailing the N-way rollback / multi-ingress fan-out.
Suggested resolution: spec the batch-failure path: every item's waiter receives the error; each ingress rolls back its own pre-Raft local state; confirm no orphan composition/chunk survives a failed batch (the existing single-path rollback, applied per item).

## Finding: Leader change mid-batch
Severity: Medium
Category: Correctness > concurrency / failure cascades
Location: ADR-046 §3/§5
Description: A batch submitted via `client_write` during a leadership change returns `ForwardToLeader`/error for the whole entry. All queued waiters must receive a retryable error and re-route (via #114 forwarding) to the new leader's queue. Unspecified.
Suggested resolution: map the batch `client_write` error to a per-item retryable `LeaderUnavailable`/`ForwardToLeader` and document the re-route (idempotency keeps the retry safe — see the corrected H2 queue-side dedup).

## Finding: Low-concurrency latency regression; adaptive-window underspecified
Severity: Low
Category: Correctness > edge cases
Location: ADR-046 §4
Description: `FLUSH_INTERVAL` adds latency when batches don't form. The "flush immediately below a threshold" mitigation lacks a precise rule (threshold value, how "forming" is detected). At depth 1 a write must not wait the full interval.
Suggested resolution: specify "flush immediately while queue depth < 2 (no batch is forming); engage the timed window only once depth ≥ 2," or an equivalent measured rule; validate with the `raft_commit` put-phase histogram.

## Finding: Per-write observability folded into one entry
Severity: Low
Category: Robustness > observability gaps
Location: ADR-046 §2
Description: One Raft entry now represents N writes; the `raft_commit` put-phase histogram and any per-entry trace no longer attribute to a single write. Acceptable, but call it out + add a `batch_size` metric so the coalescing factor is observable.
Suggested resolution: emit a `write_coalesce_batch_size` histogram; note in the ADR that per-write commit attribution is replaced by per-batch + batch-size.

---

## Recommendation
Implementation is **blocked** until C1 (mixed-version log-format gate — the concrete mechanism) and H1–H3 are resolved in an ADR rev. C1 and H3 are consensus-safety/availability; H1 is correctness; H2 is scope discipline (don't grow the replicated state machine for idempotency that already lives at the gateway). The Medium/Low items can be resolved in the same rev or tracked as implementation requirements.
