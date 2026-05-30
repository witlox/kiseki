# ADR-047: Decoupled Write Acknowledgment — Quorum-Durable Intent + Perspective Sequencing + Async Raft Ordering

**Status**: **Proposed (rev-2 2026-05-29 — adversary gate-1 Criticals resolved)**. Ready for build-phase planning + gate-2 (implementation mode). Relaxes the synchronous-commit-before-ack form of **I-L2** and **I-CS1** (both `Proposed (rev 2026-05-29)`). Gate-1: `specs/findings/2026-05-29-adv-gate1-adr047-decoupled-write-ack.md`. Resolutions in Rev-2 below.

## Problem

The write path acks only after the **synchronous Raft consensus round** (commit + apply) of the composition delta. Measured on GCP (default profile, EC-4+2, 6-shard, #137 binaries, 64 KB): native put-heavy ≈ **1,595 op/s aggregate — ~2 % of the 64 KiB target**. Per-write cost decomposes to ~12 ms EC fan-out (`chunk_write`) + ~7.65 ms Raft round (`raft_commit`), **sequential, on the ack path**. The Raft round does not shard-scale throughput: single-ingress forward funnel, per-op consensus, and a cluster-wide (not shard-local) EC fan-out.

**Root cause (the conflation):** EC/replication (*data durability*) and Raft (*metadata ordering*) were billed as one "synchronous write cost." They are orthogonal. Data durability is the **floor** (don't lose bytes). Metadata ordering is **consistency** (single global order). Synchronous-Raft-commit-before-ack is **stricter than no-loss requires** — it puts the ordering consensus on the latency-critical path when only the durability quorum must be.

## Decision

**The ack is gated solely on the quorum-durable floor (data AND metadata on `min_acks`) plus an ingress-assigned perspective sequence. Raft provides the total order and reader visibility asynchronously, recoverable by replaying the quorum-durable intent.** No acknowledged write is ever lost.

### 1. Perspective sequence (the order, assigned at ingress)
Every write is stamped at ingress (on *any* node) with a **perspective-seq** = HLC `(physical_ms, logical, node_id)`, globally total-orderable. It is carried in the intent and is the sole determinant of the eventual Raft apply order (deterministic last-writer-wins on the same name). This reuses the existing `DeltaTimestamp`/HLC; ingress updates its HLC on intent receipt (standard HLC merge) to bound skew.

### 2. Quorum-durable intent (the floor — data **and** metadata)
- **Data**: chunks EC/replicated to `min_acks` per the pool durability strategy — the existing **I-L5** floor (group-commit allowed).
- **Metadata**: the composition delta `{perspective_seq, tenant, namespace, name, chunk_ids, op_type, client_idempotency_key, inline_payload?}` is written **leaderless to a quorum (`min_acks` of the shard's RF)** of a per-shard **intent store** — a durable structure *alongside* the Raft log, NOT a Raft consensus entry.
- **Ack fires when both are `min_acks`-durable.** No consensus round, no forward-to-leader on the ack path.

### 3. Async perspective-ordered apply (the order + visibility)
The shard leader runs a committer that:
1. Gathers pending intents (its own + a quorum's) and computes a **stability watermark `W`** = the highest perspective-seq such that *every* replica in the quorum has reported it holds no un-incorporated intent with seq < W (per-replica `next_pending_seq` low-water-mark gossip on the existing heartbeat).
2. Orders all intents with seq ≤ W by perspective-seq, appends them **into the Raft log** (the canonical total order, **I-L1**), commits + applies (hydration → reader visibility), then prunes incorporated intents from the intent store.

Apply (hence visibility) is **off the ack path**; it lags by at most the stability window (bounded-stale per **I-CS2**). The watermark is what prevents a late, lower-seq intent from forcing a rollback of already-visible state.

### 4. Recovery (no loss, no rollback)
On crash / leader election the new leader runs an **intent-recovery gather**: a quorum read of the per-shard intent store reconstructs all pending intents (an intent acked on `min_acks` is seen because the new leader's quorum overlaps the intent's quorum by ≥1 with RF-3/`min_acks`=2), re-derives W, and resumes §3. Already-applied entries are in the Raft log (durable, ordered); pending intents are replayed in perspective order. No acked write is lost; no applied (visible) write is rolled back.

### 5. Idempotency
Dedup is keyed on the **client `idempotency_key`** (stable across retries and re-ingress on a different node — the server-assigned perspective-seq is NOT used for dedup because it differs per ingress). Enforced at intent-write (quorum rejects a duplicate key, returns the in-flight/first result) and again at apply (leader skips a seen key).

### 6. Per-protocol read semantics (visibility vs ack)
- **S3 / object**: eventually consistent reads within the I-CS2 staleness bound; a GET before apply may 404 (acceptable, matches the S3 contract). Read-your-writes via routing on the client-held perspective-seq.
- **POSIX / NFS / FUSE (close-to-open, ADR-013)**: read-after-close MUST see the write. The read path therefore **resolves pending intents**: a name lookup that misses the applied composition store consults the per-shard intent store for the latest pending intent for that name. close-to-open is preserved without forcing synchronous apply.

### 7. Per-class handling (the 100-node walkthrough)
| class | data floor | metadata floor | scales by |
|---|---|---|---|
| meta (every write) | — | delta intent on `min_acks` + seq | shard (≤64) |
| small (inline ≤4 KB, ADR-006) | = metadata (bytes ride the delta) | one intent on `min_acks` + seq | shard (≤64) |
| chunk-EC (large) | chunks on `min_acks` (EC, all nodes) | one delta intent on `min_acks` + seq | **node (all 100)** |

## The four safety obligations (gate-1 must verify each)
- **O1** — perspective-seq = HLC gives a deterministic global order + LWW; no split-brain even with leaderless intent. *(Depends on §1 skew bound + §3 stability watermark.)*
- **O2** — a quorum-durable intent survives leader election. *(Depends on §4 intent-recovery gather + `min_acks` quorum overlap.)*
- **O3** — crash/retry idempotency. *(Depends on §5 client-key dedup.)*
- **O4** — `min_acks` ≥ 2 (meta RF-3) and the EC `min_acks` each tolerate ≥1 failure before ack. *(Inherits the I-L5 page-cache/flush-window caveat for correlated power loss.)*

## Alternatives considered
- **Per-namespace CP-vs-AP consistency switch** — rejected: wrong axis (implies "AP namespaces accept loss"); the floor is universal no-loss.
- **W1 batched Raft commit (ADR-046)** — rejected/measured-flat: amortizes a round openraft already amortizes; keeps the consensus on the ack path.
- **Status quo (synchronous commit before ack)** — the over-strict baseline; does not shard-scale.

## Risks (for adversary gate-1)
1. Stability-watermark **liveness**: a slow/silent replica stalls W → apply stalls → unbounded visibility lag.
2. **Intent store is a new durable format** — same rolling-upgrade hazard as ADR-046 gate-1 C1 (mixed-version decode).
3. HLC **clock skew** picking the wrong LWW winner (deterministic but incorrect).
4. POSIX close-to-open **pending-intent read** correctness + cost.
5. Leaderless intent write vs **Raft single-writer** safety — the intent store ↔ Raft log relationship.
6. Intent **GC / unbounded accumulation** under apply lag.
7. I-NG2/I-NG16 **commit-on-close atomicity** semantics under async apply (visible-after-apply, not visible-after-Ok).

## Rollout
Pre-production rollout: decoupled-ack is THE write path for async-eligible surfaces (S3, Native) — no capability gate. POSIX surfaces (NFS, FUSE) keep the synchronous semantic via `WriteSurface::is_async_ack_eligible` (ADR-013/014), which is a real per-surface contract, not a back-compat flag. Observability: `intent_pending_total`, `apply_lag_seconds`, `stability_watermark_seq`.

> **2026-05-30 rip note.** The capability gate (`KISEKI_DECOUPLED_ACK` env + `DecoupledAckEnabled` cluster capability) was removed before any production deploy. Kiseki is pre-production; no migration is owed. Single-node `MemShardStore` / `PersistentShardStore` collapse `put_intent_and_fan` to a synchronous local append (`min_acks = 1` = one local copy); multi-node `RaftShardStore` does the real quorum intent-write. On a non-durable intent-store open the shard creation now PANICs (F-P5b-rpc-1: a non-durable acked intent would lose data — no silent degrade). The gateway no longer falls back to the synchronous emit on `put_intent_and_fan` failure; the error propagates to the client.

## Consequences
- **I-L2 / I-CS1 revised** (done, marked Proposed) to the quorum-durable floor + async ordering.
- New per-shard **intent store** (durable, quorum-replicated) alongside the Raft log; new async committer; leader-election intent-recovery.
- Read path gains pending-intent resolution (POSIX) / bounded-stale (S3, I-CS2).
- I-NG2/I-NG16 must be restated: CommitStream `Ok` = quorum-durable intent (no loss); visibility = post-apply (bounded-stale).

## Rev-2 (2026-05-29) — adversary gate-1 resolutions

Gate-1 returned 3C + 1H + 3M, all accepted. Resolutions:

**F-1 (watermark liveness) — majority, not all.** §3 step 1 amended: W advances on a **majority** low-water-mark (mirrors Raft commit-on-majority). A replica lagging > `T_watermark` is excluded from the watermark set and catches up via normal replication — it never stalls apply. Apply lag is bounded by backpressure (F-6 cap).

**F-2 (intent↔log crash consistency) — idempotent re-incorporation; prune is advisory.** Each Raft entry records `incorporated_seqs`. On recovery, a pending intent whose seq ≤ the log's `max_incorporated_seq` for that key is **dropped, not re-applied**. Pruning the intent store is an optimization derivable from the log — never a correctness dependency. The §5 dedup index is part of the replicated state machine (durable, snapshot-included), so a duplicate `idempotency_key` is skipped deterministically on every replica and on replay.

**F-3 (POSIX close-to-open) — per-SURFACE split (the design verdict).** Async-apply and POSIX close-to-open are irreconcilable, so the relaxation is **scoped by protocol surface, by each protocol's own consistency contract — not an arbitrary per-namespace knob**:
- **S3 / object / native**: async-apply; reads bounded-stale within I-CS2; read-your-writes via the client-held perspective-seq.
- **POSIX / NFS / FUSE**: **synchronous-apply** — `close()`/CommitStream blocks until the intent is applied (visible) on the owning shard, preserving close-to-open (ADR-013). These surfaces keep the strict path and do **not** get the write-ack relaxation (they still benefit from #137 + EC/Raft parallelization, just not the async ack).

This split is the decision of record here; it is cross-referenced into **ADR-013** (POSIX: synchronous-apply required) and **ADR-014** (S3: async/bounded-stale permitted), and the invariants carry a per-surface qualifier (I-CS1, I-NG2). See "Follow-ups" below.

**F-4 (mixed-version format) — N/A pre-production.** The capability gate was retired (2026-05-30 rip): kiseki is pre-production and no node fleet predates the intent-store format. Decoupled-ack is THE async-surface write path; there is nothing to flip off. Snapshots still include the dedup index + un-incorporated intents.

**O1 skew** — max-skew bound `B`: an intent whose `physical_ms` exceeds local HLC by > B is clamped (logical-extended) + alerted; a node beyond B (NTP failure) is fenced from ingress.
**O2 recovery** — the new leader completes intent-recovery (quorum gather + W re-derivation) **before** resuming apply; openraft leadership is necessary, not sufficient. Election concurrent with a membership change gathers from the intersection of old+new config quorums; no apply until the config is stable.
**O3 dedup** — keyed on client `idempotency_key`, durable + replicated (per F-2); the double-fan-out's data side is absorbed by content-addressed chunk dedup (identical `chunk_id`).
**O4 intent durability** — the metadata intent follows the pool durability strategy + the I-L5 group-commit window by default (page-cache on `min_acks`; correlated-power-loss window = `KISEKI_CHUNK_FLUSH_INTERVAL_MS`); POSIX/synchronous surfaces and stricter pools use `sync_per_write=true`. Disclosed identically to I-L5.

### Follow-ups (tracked)
1. **ADR-013** amendment — POSIX/NFS/FUSE require synchronous-apply (close-to-open); record the per-surface split.
2. **ADR-014** amendment — S3/object async/bounded-stale permitted (matches the S3 contract).
3. **Invariant restates** — I-NG2/I-NG16 (CommitStream `Ok` = durable; visibility = post-apply within I-CS2); I-CS1 per-surface qualifier; I-CS3 ("per-site CP" wording, now stale); ADR-032 §write-path + ADR-026 (describe the current synchronous-only mandate — note the relaxation).
4. **Build phase** — intent store (durable, quorum-replicated, format-versioned), majority-watermark committer, idempotent re-incorporation, intent-recovery on election, per-surface read path (POSIX sync-apply / S3 bounded-stale + pending-intent resolve), observability (`intent_pending_*`, `apply_lag_seconds`, `stability_watermark_seq`). (No capability gate — see 2026-05-30 rip note above.)
5. **Adversary gate-2** (implementation mode) once code exists.
