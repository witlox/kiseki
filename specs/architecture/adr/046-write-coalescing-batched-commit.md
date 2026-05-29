# ADR-046: Write Coalescing — Batched Raft Commit for the Small-Object Write Path (W1)

**Status**: **❌ REJECTED (2026-05-29) — built, measured flat, reverted.** The design was implemented (Release R + R+1) and measured on local 3-node + GCP 6-node: **no throughput lift**. Root cause: **openraft already auto-batches concurrent `client_write`s** (`max_payload_entries: 300`) — proven by 13× throughput scaling conc-1→16 *without* W1 — so coalescing into a single `BatchChunkAndDelta` entry amortises a consensus round that openraft already amortises. And the round itself is only **~1 ms** (isolated probe `multi_shard_transport::measure_openraft_round_latency`), not the 30–45 ms this ADR assumed, so the per-entry/apply saving W1 *does* provide is noise. The real write bottleneck is a **stacked off-CPU wait** in the full-server commit pipeline (~15 ms local / ~42 ms GCP), mechanism still unproven — see `docs/performance/roadmap.md`. Code reverted on `perf/2026-05-28-roadmap`; this ADR retained as the rejected-decision record. *Design + adversary gate-1 + rev-2 below are preserved as history.*

*(original)* Status: Proposed — adversary gate-1 complete 2026-05-28 (1C + 3H + 3M + 2L; see `specs/findings/2026-05-28-adv-gate1-adr046-write-coalescing.md`), all resolved in **rev-2** below.
**Date**: 2026-05-28 (first draft); 2026-05-28 (rev-2 — gate-1 resolutions)
**Deciders**: Architect (this ADR), Adversary (gate-1)
**Context**: ADR-026 (Raft topology), ADR-033 (shard topology), ADR-040 (composition store; rev-4 retired the write-behind queue), ADR-041 (multiplexed Raft transport — one listener per node, groups register via the control-plane apply hook), ADR-042 §14 (native per-binding throughput targets), #111/#114 (forward-to-leader write path), #126 (commit-bound writes), I-CP1 (composition store advances `last_applied_seq` only inside the same transaction that applies the state change), I-L5 (composition durability), `docs/performance/roadmap.md` (W1), `docs/performance/targets.md` (default: native TCP-framed PUT aggregate 360 k op/s).

## Problem

Multi-node writes are **commit-bound**. A single PUT (S3 / native / NFS COMMIT) costs ~p50 180 ms and the 6-node `default` cluster tops out at **~250 op/s · ~15 MB/s** aggregate, versus a derived target of **360 k op/s** (native, small object) / 5.2 GB/s (S3 PUT, large object). Reads scale (23 k op/s · 1.4 GiB/s).

Root cause (traced through `mem_gateway::write` → `log_bridge` → `append_forwarder` → `kiseki-log/src/raft/openraft_store.rs`): **every write is its own Raft log entry, submitted via `self.raft.client_write(cmd).await` and awaited synchronously — one consensus round per write.** openraft already batches the *log fsync* per AppendEntries and pipelines replication (`max_payload_entries: 300`), and the Raft log store batches its fsync (`fjall_raft_log_store.rs`), so the durability layer is not the gap. The gap is that **the state-machine apply is serial per shard** and there is **no coalescing of independent writes into a shared consensus entry** — concurrent writes on a shard queue behind one another, so per-shard throughput is bounded by `1 / apply-and-commit-latency`, not by NIC/disk/CPU.

This regime is specifically the **small-object op/s** target. Large-object bandwidth (S3 PUT 5.2 GB/s) amortizes the single commit over the object and is bounded by the data path, not consensus (see roadmap #2). Read targets take no consensus cost (roadmap #1). **W1 is the small-object write lever only.**

## Decision

Introduce **write coalescing**: at a shard leader, accumulate independent `ChunkAndDelta` proposals arriving within a bounded window into a **single Raft log entry**, commit them in one consensus round, and fan the per-item results back to each waiting writer.

### Why this is a new ADR and not ADR-026 / not an implementation detail

ADR-026 already anticipates "batching" — but its **Strategy C** is explicitly a *transport* optimization: "Coalesce heartbeats per node pair… **pure transport change, no protocol change**" (ADR-026 §Transport / §Migration). That coalesces the *heartbeat/AppendEntries RPCs between node pairs* and leaves the log-entry-per-write model untouched.

W1 is the orthogonal, **protocol-level** change: it coalesces *independent client writes into one log entry* — a new `LogCommand` variant, a new apply-atomicity shape (N deltas/entry), a new per-item idempotency/result-fan-out contract, and a new durability/latency window. It is **not** covered by ADR-026 (which disclaims protocol change), and it is **not** a pure implementation detail (it alters the wire command set — a mixed-version-cluster concern — and the commit contract). It therefore needs ADR-level review. It **extends** the `ChunkAndDelta` atomicity contract owned by ADR-040 / `phase-16-cross-node-chunks.md` (D-4/D-10) and composes with ADR-041's many-groups-per-node transport. The two batchings are complementary and could both apply at 100+ nodes (ADR-026-C coalesces the RPCs; ADR-046 coalesces the entries inside them).

### 1. New command — `LogCommand::BatchChunkAndDelta`

```rust
LogCommand::BatchChunkAndDelta { items: Vec<ChunkAndDeltaItem> }
```
where `ChunkAndDeltaItem` carries exactly the fields of today's `LogCommand::ChunkAndDelta` (tenant, operation, hashed_key, chunk_refs, payload, has_inline_data, new_chunks). One log entry carries N items. **No `idempotency_key` rides the command** — idempotency stays gateway/queue-side where it lives today (rev-2 / gate-1 H2; the replicated state machine holds no idempotency state and gains none). The existing single `ChunkAndDelta` is retained (a batch of one is still valid; old replicas that don't understand the batch variant are handled by the **mixed-version gate**, rev-2 §C1).

### 2. Apply is atomic and per-item-sequenced

`apply_to_sm` for `BatchChunkAndDelta` applies all N items **in batch order, in one state-machine apply step**:
- Raft guarantees the *entry* is all-or-nothing, so either all N items apply or none — this is the atomicity the gateway's "ack after commit" contract relies on, preserved per item.
- The shard's **delta sequence** advances by N (one delta per item); the Raft **log index** advances by 1 (one entry). I-CP1 is preserved: the N state changes + the new `last_applied_seq` (= the entry's effect) commit in the same transaction the single `ChunkAndDelta` already uses — the batch just makes "the state change" be N deltas instead of one.
- Apply returns `Vec<LogResponse::Appended(seq)>` — one delta-seq per item, in batch order.
- **Per-item idempotency is queue-side, NOT in apply** (rev-2 / gate-1 H2 — `apply_command` has no idempotency check today; it must not grow one). The coalescing queue dedups by `idempotency_key` *before* an item enters a batch: a duplicate is collapsed and its waiter is resolved with the first/in-flight item's result. The replicated apply stays a pure append.
- **Per-item HLC (rev-2 / gate-1 H1):** `append_delta_inner` stamps the delta HLC from `log_index` (`physical_ms = log_index, logical = 0`). All N items in a batch share one `log_index`, so the batch apply MUST pass an incrementing `logical = item-index` (0..N-1); the N deltas are then `(physical_ms = log_index, logical = 0..N-1)` — distinct, deterministic on replay, and monotonic for any same-key items in the batch. Without this, batched deltas are HLC-indistinguishable.

### 3. The coalescing queue (per shard, leader-side)

A per-shard `mpsc` of `(ChunkAndDeltaItem, oneshot::Sender<Result<SequenceNumber, LogError>>)` plus a flush task:
- Writers (`append_chunk_and_delta_with_forwarding`, after the #114 forward lands the item on the leader) `send` their item + a oneshot and `await` the oneshot.
- The flush task drains the queue and submits one `client_write(BatchChunkAndDelta)` when **either** `MAX_BATCH` items are queued **or** `FLUSH_INTERVAL` elapses since the first queued item — whichever first.
- On commit, it matches the `Vec<seq>` back to the queued oneshots in order and resolves each.
- Bounded queue depth → backpressure (a full queue blocks the sender, the natural flow-control).

### 4. Latency floor / adaptive window

A write now waits up to `FLUSH_INTERVAL` for the batch to fill. At low concurrency this *adds* latency. Mitigation: **flush immediately when the queue is below a small threshold** (no benefit to waiting for a batch that isn't forming) and only engage the timed window under load. `MAX_BATCH` and `FLUSH_INTERVAL` are tunable (`KISEKI_WRITE_COALESCE_MAX_BATCH`, `_FLUSH_US`).

### 5. Interaction with #114 forwarding and #132

Forwarded writes (a PUT landing on a non-leader → `AppendChunkAndDelta` to the leader) enter the **same** leader-side queue, so coalescing applies uniformly to direct and forwarded writes. (Note #132: the forwarded path replicates via Raft, not fabric `PutFragment` quorum — orthogonal to this ADR.)

## Expected lift + how the target is reached

Per shard, throughput goes from `1 / commit-latency` (~25–40 op/s observed) to `MAX_BATCH / commit-latency` (~`64 ×` at batch 64). With 6 distributed-leader shards: ~250 → **~10 k op/s** aggregate (~40×).

Reaching the **360 k op/s** target then needs **sharding at scale**: `360 k ÷ (~1.5 k/shard-with-coalescing) ≈ ~240 Raft groups` (~40/node). ADR-041's multiplexed transport already supports many groups per node (one listener, control-plane apply-hook registration); the practical ceiling is per-group heartbeat/election/apply overhead, which bounds groups/node — so 360 k is the aggressive end and may require larger batches or a higher group count than is comfortable. Honest framing: **W1 moves writes into the tens-of-k order; 360 k is a stretch that depends on how far sharding scales before group overhead dominates.**

## Alternatives considered

- **Pipeline only (raise openraft in-flight, no coalescing):** the per-shard *apply* is serial regardless, so pipelining the *replication* doesn't raise the commit rate. Coalescing reduces the number of entries/applies — strictly better for op-rate. Rejected as insufficient alone.
- **Relax durability (ack before quorum):** breaks I-L5. Rejected.
- **More shards alone (no coalescing):** linear in shards but each shard stays at ~25 op/s; coalescing × shards is the multiplier. Complementary, not a substitute.

## Risks (for adversary gate-1)

1. **Per-item result fan-out** — oneshot lifecycle: dropped waiters (client gone), commit-failure fan-out (all N items get the error), timeouts. A leaked/forgotten oneshot must not wedge the flush task.
2. **Apply atomicity + seq assignment** — N deterministic delta-seqs per entry; replay on a follower / snapshot install must assign identical seqs. Audit against I-CP1 and the snapshot/restore path (`state_machine.rs::build_snapshot` / `apply`).
3. **Idempotency within a batch** — two items with the same `idempotency_key` in one batch (double-submit racing into the same window); define the tie-break (first wins, second gets the first's result).
4. **Crash between entry-commit and gateway-ack** — same window as today's single `ChunkAndDelta` (the entry is durable; the gateway re-derives on restart) — confirm no new window opens per-item.
5. **Latency regression at low concurrency** — the adaptive-window mitigation must be measured (the `raft_commit` put-phase histogram, added 2026-05-28, decomposes this).
6. **Mixed-version cluster** — a replica that doesn't understand `BatchChunkAndDelta` must not silently drop it (see Rollout gating).

## Rollout

1. **Phase 1** — land the command variant + atomic apply + per-shard queue + fan-out behind `KISEKI_WRITE_COALESCE=off` (default). Single-entry path unchanged when off. All replicas in a cluster must run a binary that *understands* the batch variant before any leader emits one (the variant decodes on every replica; gate on a cluster-min-version check or a control-plane feature flag, never emit to a cluster with an older replica).
2. **Phase 2** — enable on a test cluster; read the `raft_commit` put-phase histogram + aggregate op/s; tune `MAX_BATCH` / `FLUSH_INTERVAL`.
3. **Phase 3** — re-measure the downstream items the roadmap flagged as *masked by W1*: chunk-write parallelism (roadmap #2) and hydrator throughput (#133) only become observable once writes are fast enough to produce deltas at a rate that stresses them.

## Rev-2 (2026-05-28) — adversary gate-1 resolutions

Gate-1 raised 1C + 3H + 3M + 2L (`specs/findings/2026-05-28-adv-gate1-adr046-write-coalescing.md`). Resolutions:

**C1 — Mixed-version log-format gate (the blocker).** `BatchChunkAndDelta` is a new `LogCommand` variant written into the durable Raft log; a replica on older code cannot deserialize it → state-machine divergence/crash during a rolling upgrade. Two-release migration, mandatory:
1. **Release R (decode-only):** every node ships the `BatchChunkAndDelta` *decoder* and apply path, but **no leader ever emits it** (the coalescing queue is compiled in but hard-gated off). After R is fully rolled out, every replica can apply a batch entry if it sees one.
2. **Release R+1 (emit-gated):** a leader emits batches **only when a cluster-wide `WriteCoalesceEnabled` capability is committed ON**. That capability is set through the control-plane apply path (alongside `SetShardConfig`) and flips ON only after every node has reported, via the control plane / heartbeat capability field, that it runs ≥ R. A node that does not advertise the capability keeps the flag OFF cluster-wide; a non-advertising node joining flips it OFF again (fail-safe). `KISEKI_WRITE_COALESCE` only *permits* emission; the committed capability is what *authorizes* it.
3. **Snapshots:** a snapshot whose state was produced by batch entries is still just composition/chunk state (no new format) — but the snapshot's log-entry replay on install must run on ≥ R. Gate snapshot install the same way (a < R node refuses to join a cluster that has emitted batches). Document in ADR-040's snapshot section.

**H1 — per-item HLC `logical`.** Resolved inline in §2: the batch apply passes `logical = item-index`.

**H2 — idempotency stays gateway/queue-side.** Resolved inline in §1/§2: no `idempotency_key` in the command; the queue dedups before batching. The replicated state machine gains no idempotency state.

**H3 — batch byte cap.** §3's flush trigger gains `MAX_BATCH_BYTES` (default a safe fraction of the transport max-message — the fabric's ~64 MiB cap minus envelope headroom): flush on whichever of `MAX_BATCH` (count) / `MAX_BATCH_BYTES` / `FLUSH_INTERVAL` trips first. A single item larger than `MAX_BATCH_BYTES` is submitted as a singleton (never coalesced). This keeps every `AppendEntries`/log record under the transport limit.

**M1 — flush-task liveness + oneshot lifecycle.** §3 gains: a per-item submit timeout (the writer gives up with a retryable error; the flush task tolerates a dropped `oneshot` receiver — send-error is ignored, not fatal); the flush task is supervised/restartable (a panic must not wedge the shard — re-spawn + fail in-flight waiters with a retryable error); bounded queue overflow returns a retryable `Unavailable` (backpressure), never blocks unboundedly.

**M2 — N-way commit-failure rollback + multi-ingress fan-out.** On batch `client_write` failure, every item's waiter receives the error and each **ingress node** rolls back its own pre-Raft local composition create (the existing single-write rollback, applied per item; for #114-forwarded items the ingress is the original node, which rolls back on receiving the forwarded error). No orphan composition/chunk may survive a failed batch — the existing per-item rollback invariant is preserved, just fanned to N.

**M3 — leader change mid-batch.** A batch submitted during a leadership change returns `ForwardToLeader`/`LeaderUnavailable` for the whole entry; the queue maps it to a per-item retryable error and each item re-routes through #114 to the new leader's queue. Queue-side idempotency (H2) makes the retry safe.

**L1 — low-concurrency latency.** §4 made precise: **flush immediately while queue depth < 2** (no batch is forming); engage the timed window only at depth ≥ 2. A depth-1 write never waits `FLUSH_INTERVAL`.

**L2 — observability.** Add a `kiseki_write_coalesce_batch_size` histogram so the coalescing factor is measurable; note that per-write commit attribution is replaced by per-batch + batch-size (the `raft_commit` put-phase histogram now times a batch).

**Phasing impact:** C1 makes this a **two-release** feature. Phase-1 impl (Release R) is decode + apply + the queue, hard-gated off — landable + testable in isolation without affecting any running cluster. Emission (Release R+1) is gated on the committed capability.

## Consequences

- Unblocks the small-object native op/s target (the one regime W1 addresses).
- The write→commit contract gains a batch entry type; the durability/atomicity guarantees are unchanged per item but the apply path and the gateway result-handling are more complex.
- After this lands, the next bottlenecks to profile are the EC read path (already improved, roadmap #1), large-object write parallelism (#2), and hydrator throughput under the new write rate (#133).
