# ADR-046: Write Coalescing — Batched Raft Commit for the Small-Object Write Path (W1)

**Status**: Proposed (needs adversary gate-1 before implementation)
**Date**: 2026-05-28
**Deciders**: Architect (this ADR)
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
where `ChunkAndDeltaItem` carries exactly the fields of today's `LogCommand::ChunkAndDelta` (tenant, operation, hashed_key, chunk_refs, payload, has_inline_data, new_chunks) **plus** the per-item `idempotency_key`. One log entry carries N items. The existing single `ChunkAndDelta` is retained (a batch of one is still valid; bindings/old replicas that don't understand the batch variant must be gated — see Rollout).

### 2. Apply is atomic and per-item-sequenced

`apply_to_sm` for `BatchChunkAndDelta` applies all N items **in batch order, in one state-machine apply step**:
- Raft guarantees the *entry* is all-or-nothing, so either all N items apply or none — this is the atomicity the gateway's "ack after commit" contract relies on, preserved per item.
- The shard's **delta sequence** advances by N (one delta per item); the Raft **log index** advances by 1 (one entry). I-CP1 is preserved: the N state changes + the new `last_applied_seq` (= the entry's effect) commit in the same transaction the single `ChunkAndDelta` already uses — the batch just makes "the state change" be N deltas instead of one.
- Apply returns `Vec<LogResponse::Appended(seq)>` — one delta-seq per item, in batch order.
- **Per-item idempotency** is unchanged: each item dedups on its own `idempotency_key` during apply; a re-sent item that was already applied is a no-op for that slot (and its waiter still gets the prior seq/comp_id). Idempotency does NOT span items.

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

## Consequences

- Unblocks the small-object native op/s target (the one regime W1 addresses).
- The write→commit contract gains a batch entry type; the durability/atomicity guarantees are unchanged per item but the apply path and the gateway result-handling are more complex.
- After this lands, the next bottlenecks to profile are the EC read path (already improved, roadmap #1), large-object write parallelism (#2), and hydrator throughput under the new write rate (#133).
