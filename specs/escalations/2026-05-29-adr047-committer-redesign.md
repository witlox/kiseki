# ADR-047 incorporation/commit mechanism — design deliberation (2026-05-29)

The decoupled-ack ack-path works, but the **incorporation** mechanism (getting a
quorum-durable intent into the per-shard Raft total order so it becomes
reader-visible) is broken in multi-node testing. This doc is a diamond-workflow
deliberation: analyst framing → architect design → adversary review → synthesis.

The breaks (test: PUT node-1, GET node-2 → 404 for 10s):
- B1 (impl, separable): committer peer-gather can't reach peers (Connection reset / shard_retired).
- B2 (design): min_acks=2 vs 6-voter majority=4 → an intent on 2 is never reported by a majority.
- B3 (design): exclusive watermark (seq < W) → the latest/only pending intent never drains when idle.
- B4 (design, latent): committer drains only the LOCAL store; an intent fanned to a subset excluding the leader is never incorporated by anyone.

---

## PART 1 — ANALYST FRAMING

**Requirements (each tied to an invariant):**
- R1 no-loss: every acked (min_acks-durable) intent is eventually incorporated, across leader elections (I-L2/I-CS1/O2). *The current mechanism violates this.*
- R2 ordering: one recoverable per-shard total order, no split-brain, per-key LWW correct (I-L1/O1). **Open crux R2(a): must the order equal perspective-seq order, or only be consistent with per-key happens-before (LWW by HLC)? I-L1 says "a total order exists," NOT "it equals perspective-seq."**
- R3 idempotency / at-most-once incorporation (F-2 floor, O3 client key).
- R4 liveness: bounded-time visibility incl. idle shards + slow minority (I-CS2/F-1). B2/B3 violate this.
- R5 crash/election: new leader reconstructs the complete acked set before resuming; no rollback of visible state (O2/F-2).
- R6 data-before-metadata preserved under partial-intent recovery (I-L5).

**The load-bearing decision:** resolve R2(a). If "per-key LWW only," the stability watermark (and B2/B3) is unnecessary → Option (ii) opens. If "full per-shard perspective-seq order is a contract," a reordering barrier is unavoidable.

**Key tensions:** (b) single-committer (leader-only) vs every-node (today: every-node, only leader's append lands — wasteful, amplified B1); (c) who incorporates an intent fanned to a subset excluding the leader (B4) — leader-pull / fan-includes-leader / route-to-leader; (d) must min_acks align with the Raft voter majority? (today divorced: 2 vs 4 = B2); (e) composition is written LOCALLY on ingress → incorporation is the ONLY path by which other nodes learn the write (so a stalled committer = permanent invisibility, not bounded-stale).

**Option space (unranked):**
- (i) Leader-pull gather-all-then-order: leader pulls full pending set from a quorum, orders, incorporates. Fixes B4/B2. Risk: bandwidth (need delta-pull); still needs a barrier if R2(a)=full-order.
- (ii) Incorporate-on-min_acks-durable, drop the watermark (needs R2(a)=LWW-only): incorporate each acked intent as it arrives, LWW-by-HLC at apply. Kills B2/B3. Risk: unsound if anything needs monotone per-shard seq; still needs B4's answer (who appends).
- (iii) Route-intent-to-leader: small metadata intent sent to leader, leader is sole incorporator. Fixes B4 by construction, no watermark. Risk: re-introduces a forward-to-leader hop (erodes perf win, but a small-metadata RTT << a full sync Raft round); leader hot-spot.
- (iv) Fix the watermark in place: inclusive bound + idle-flush + min_acks alignment + B4 fix + B1. Fixes all, minimal change, reuses code. Risk: preserves the suspect parallel-quorum frame; min_acks alignment erodes the perf win; largest live surface.

**Non-negotiable:** R1, R3, R2(one correct order/no split-brain/per-key LWW), R4 (no permanent invisibility), R5, R6, surface scope (S3+Native async only; POSIX/NFS/FUSE stay synchronous). **Tradeable:** min_acks=2, fully-leaderless ingress, tick latency, reusing existing code (don't let sunk cost anchor the choice).

---

## PART 2 — ARCHITECT DESIGN

### (1) R2(a) finding — **per-key LWW (by perspective-seq) is sufficient; full per-shard perspective-seq order is NOT a contract.**

Decisive, from the code that actually consumes the order:

- **Invariant text.** I-L1: *"Within a shard, deltas have a total order."* (Confirmed) — it asserts a total order **exists**, not that it equals perspective-seq order. I-T7 (Confirmed): *"Intra-shard ordering uses Raft sequence numbers (total order)."* **The Raft log index is the order of record.** The HLC/perspective-seq is the LWW tie-break, not the global sequence.
- **The state machine assigns its own monotone sequence at apply.** `append_delta_inner` (`crates/kiseki-log/src/raft/state_machine.rs:268`) does `self.tip += 1; let next_seq = SequenceNumber(self.tip)` and stamps the delta's HLC `physical_ms = log_index` (lines 277-288). The committer-assigned perspective-seq is recorded **separately** as `max_incorporated_seq` (lines 418-442) purely as the F-2 floor — it is *not* the delta's order key. So whatever interleaving the leader appends in, the per-shard sequence is the Raft log order, full stop.
- **Read path / name resolution is pure LWW by current binding.** GET-by-key: `s3_server.rs:627` → `lookup_object_by_name` → `MemoryStorage::name_lookup((ns,name)) → comp_id` → `get(comp_id)` (`persistent/storage.rs:388-394`). The name index is a `(ns,name)→comp_id` map with **overwrite-replace** semantics (`name_insert`, lines 403-428). A name resolves to *the latest binding*, with no perspective-seq involved.
- **The hydrator (the failing "name index isn't replicating" path) is per-composition / per-name LWW.** `crates/kiseki-composition/src/hydrator.rs`: it reads `read_deltas(from = last_applied+1 .. )` *only to advance a high-water-mark so it doesn't re-read* — the `from..to` range is a cursor, not an ordering contract. Each delta is staged by `comp_id` (`stage_create`/`stage_update`/`stage_delete`) and by `(ns,name)` (`bind_name`/`unbind_name`); every step is explicitly **idempotent** (`stage_update` no-ops when `chunks==`/`size==` match, line 608; `stage_create` re-binds the name and returns `Applied` if the comp already exists, lines 534-539). The last delta touching a `comp_id`/name wins. It never requires monotone perspective-seq; it requires only that the **Raft log order** for a given name be the LWW order — which it is, by construction, because the leader appends in *some* order and that IS the order of record.
- **Compaction collapses LWW by the LOCAL Raft sequence.** `compact_deltas` (`crates/kiseki-log/src/compaction_worker.rs:90-135`) groups by `hashed_key`, sorts **`by d.header.sequence` (the Raft sequence) newest-first**, keeps the newest `min_versions`, drops tombstones. **The compaction LWW winner is decided by the Raft log index, not perspective-seq or HLC.** This is the load-bearing fact: *the order the leader appends in is the order that survives compaction and the order readers see.* As long as that order is **deterministic and identical on every replica** (which Raft guarantees — every replica applies the same log) and respects **data-before-metadata** within a single write, correctness holds.
- **Watermark/GC is keyed on the local Raft sequence** (`watermark.rs`: `gc_boundary = min(consumer positions)` over `SequenceNumber`), again the Raft sequence, not perspective-seq.

**Conclusion.** Nothing in the read, replication, hydration, compaction, or GC paths requires the Raft apply order to equal perspective-seq order. They require: (R2-i) **one** order, identical on every replica — Raft gives this for free once an entry is appended; (R2-ii) for the **same name**, the later write wins — satisfied if same-name writes are appended in perspective-seq order (or, equivalently, resolved LWW-by-perspective-seq at apply). Inter-key order is **free** — independent keys never interact in any consumer. **R2(a) = per-key LWW by perspective-seq is sufficient.** This eliminates the stability watermark (B2/B3 are artifacts of enforcing an order nobody needs).

Residual obligation that LWW-only must still honor (R6 / I-L5): a *single* write's chunks must be durable before its metadata delta is visible. That is a per-write data-before-metadata ordering, already guaranteed by the producer (fan chunks to `min_acks` before writing the intent) and preserved here — it is **not** a cross-write total-order requirement.

---

### (2) The chosen mechanism — **LeaderSink: route-intent-to-leader, single-incorporator, no watermark** (option iii, hardened)

**One-paragraph summary.** Drop the stability watermark entirely. Keep the durable per-shard `IntentStore`, the `PerspectiveSeq`, the `IncorporateIntent` Raft command, the `max_incorporated_seq` F-2 floor, and `recover_pending` — discard `compute_stability_watermark`, the majority-gather-for-watermark, and the every-node committer. The ingress node fast-acks on the `min_acks`-durable floor exactly as today (`put_intent_and_fan`), then, **off the ack path**, forwards a copy of the intent to the **current shard leader** via a new `IntentForward` RPC. The leader is the **sole incorporator**: it holds the intent in its own durable `IntentStore`, and a single leader-only committer loop drains its store into the Raft log in ascending perspective-seq order, applying each as an `IncorporateIntent` command (which de-dups by the `max_incorporated_seq` floor and by client `idempotency_key` in the state machine). Because there is exactly one writer (the Raft leader) appending in perspective-seq order, the Raft log order is a per-key-LWW-correct total order with zero coordination — no watermark, no quorum gather in steady state. Recovery on election is the existing `recover_pending` majority-gather (O2), run once before the new leader resumes incorporating; the F-2 floor makes it idempotent against the log it inherits.

**Why single-incorporator-on-leader is correct *and* simplest.** The fatal complexity in the current design is that incorporation is leaderless-but-only-leader's-append-lands (every node runs a committer, all but one wasted — tension (b)), combined with a watermark whose only job is to stop a *late lower-seq* intent from forcing a rollback of already-visible state. With LWW-only (finding 1), **there is no rollback to prevent**: a late lower-seq write for a name simply loses to the already-applied higher-seq write at apply time (the state machine compares and keeps the winner). So the watermark's entire reason to exist evaporates. Once you don't need a watermark, you don't need a quorum gather in steady state, and the cleanest way to get *one* deterministic append order is *one* writer — which Raft already elects. Routing the intent to that one writer is a small-metadata RTT (≪ a synchronous Raft round, and **off the ack path**), and it makes B4 impossible by construction.

**Diagram-in-prose — steady-state path (PUT on node-X, a follower; leader is node-L):**

```
client --PUT--> node-X gateway (ingress)
  1. EC/replicate chunks to min_acks            (data floor, I-L5)   [on ack path]
  2. stamp perspective_seq = HLC(now, X)        (ingress order)      [on ack path]
  3. put_intent_and_fan(intent):                                     [on ack path]
       - write LOCAL durable IntentStore (copy 1)
       - fan INTENT_PUT to voter peers; stop at min_acks acks
  4. ACK client  <-- fires here (min_acks-durable, no Raft round)
  --- everything below is OFF the ack path ---
  5. node-X sends IntentForward(intent) to the current shard leader node-L
       (fire-and-forget with retry; idempotent on the leader by perspective_seq + idem_key)
  6. node-L records intent in ITS durable IntentStore
  7. node-L committer loop (single, leader-only) drains its store ascending by
     perspective_seq, dropping any seq <= max_incorporated_seq (F-2):
       append IncorporateIntent{..., perspective_seq} to the Raft log
  8. Raft replicates + applies on ALL voters (L and X and the rest):
       state machine appends the delta (tip++), records max_incorporated_seq,
       dedups by idempotency_key
  9. each node's hydrator polls the applied log -> installs comp + (ns,name) binding
 10. GET-by-key on node-Y now resolves (ns,name) -> comp_id -> composition  [visible]
```

**Bounded-time visibility, idle or busy.** Step 5 is triggered *immediately* on every successful `put_intent_and_fan` (event-driven, not poll-driven) — so even a single write on an otherwise-idle shard reaches the leader at once; the leader's committer drains on a short tick (or is also event-woken) and appends a *single* `IncorporateIntent`. There is no watermark to "drain" and no "wait for a quorum to close a window" — the one pending intent goes straight in. This is precisely what kills B3 (idle never drains): an idle shard with one pending intent incorporates it on the next leader tick, bounded by `tick_interval` (default ≤ the hydrator's 100 ms cadence), independent of any other replica's pending state.

---

### (3) R1–R6 satisfaction table

| Req | How LeaderSink satisfies it |
|---|---|
| **R1 no-loss** (every acked intent eventually incorporated, across elections — I-L2/O2) | Ack fires only on `min_acks`-durable (step 4). Incorporation is via two independent routes, both converging on the leader's store: (a) steady-state `IntentForward` from ingress; (b) on any election, `recover_pending` majority-gathers the union of all replicas' pending stores into the new leader (O2 overlap proven in §6). So an acked intent is *either* already forwarded+incorporated, *or* sitting `min_acks`-durable and re-gathered by the next leader. The forward in (a) is best-effort+retried but is **not** the no-loss guarantee — (b) is. Loss would require both the forward to never land **and** the intent to vanish from a majority of stores; the latter contradicts `min_acks`-durability. |
| **R2 ordering** (one recoverable order, no split-brain, per-key LWW — I-L1/O1) | Single writer (the Raft leader) appends in ascending perspective-seq → Raft log order is a total order (I-L1), identical on every replica. Same-name LWW: appends in perspective-seq order, and the state machine additionally drops any `IncorporateIntent` whose `perspective_seq <= max_incorporated_seq` is *not* the mechanism here — see §(2) ordering note below. Inter-key order is free (finding 1). No split-brain: only the Raft leader can append; a deposed leader's appends are rejected by openraft term-fencing (§7). |
| **R3 idempotency / at-most-once** (F-2, O3) | Two layers, both in the replicated state machine so they hold on every replica and on replay: (i) `max_incorporated_seq` floor — an `IncorporateIntent` with `perspective_seq <= floor` is a no-op (already in `state_machine.rs` apply; we make the floor a *skip* not just a max-record, see §(2) note); (ii) client `idempotency_key` dedup index, snapshot-included, so a re-ingressed retry collapses to one delta. The `IntentStore::put` Duplicate path (O3) already de-dups at the durable floor. |
| **R4 liveness** (bounded visibility incl. idle + slow minority — I-CS2/F-1) | No watermark ⇒ no dependency on *any* peer reporting. The leader incorporates from its **own** store on its own tick; a slow/silent minority cannot stall it (kills B2/F-1 structurally — there is no majority-low-water-mark to drag down). Idle shard: one pending intent incorporates on the next tick (kills B3). Visibility lag = `IntentForward` RTT + leader tick + Raft round + hydrator poll — all bounded, all off the ack path (I-CS2 bounded-stale for S3/native). |
| **R5 crash/election** (reconstruct acked set, no rollback of visible state — O2/F-2) | New leader runs `recover_pending` (majority gather, §6 math) **before** resuming incorporation (O2: "leadership necessary, not sufficient"). Restored intents are re-incorporated; the F-2 floor + idem index drop anything already in the inherited log (no double-incorporation). No rollback: applied deltas are immutable in the Raft log (I-L3); a late lower-seq recovered intent for an already-written name simply loses LWW at apply and produces no visible change. Detailed in §(5). |
| **R6 data-before-metadata under partial recovery** (I-L5) | Preserved unchanged: the producer fans **chunks to `min_acks` before** writing the intent (existing `put_intent_and_fan` ordering). Recovery is at-most the set of intents that were at least locally durable, each of which had its chunks fanned first. So even an at-least-once recovered partial intent composes over already-durable chunks. LWW-only does **not** weaken this — it is a per-write ordering, untouched by dropping the cross-write watermark. |

**§(2) ordering note (the one state-machine change).** Today `IncorporateIntent` apply *records* `max_incorporated_seq = max(cur, seq)` but still appends the delta unconditionally (`state_machine.rs:418-443`). For LeaderSink the floor must also **gate the append**: if `perspective_seq <= max_incorporated_seq`, skip the append entirely (idempotent no-op), else append and advance the floor. This is the F-2 floor doing real work (it currently only feeds recovery). Same-name LWW within the *pending* set is handled by the leader appending ascending-by-perspective-seq; a same-name pair already in the log vs. a late recovered lower-seq is handled by the floor-skip. (A late recovered *higher*-seq same-name write correctly appends and supersedes — that is LWW working, not a rollback.)

---

### (4) B1–B4 elimination

- **B1 (impl, separable — committer peer-gather: Connection reset / shard_retired).** B1 lives in the *peer-gather* transport (`TransportIntentGatherer` fanning `INTENT_NEXT_PENDING` / `INTENT_GATHER_PENDING` to voter peers). LeaderSink **deletes the steady-state peer-gather** (no watermark ⇒ no `gather_next_pending_seqs`). The gather survives **only** on the recovery path (`recover_pending` via `INTENT_GATHER_PENDING`), which runs once per election, not per tick — so B1's blast radius shrinks from "every committer tick on every node" to "once at election." The transport bug itself (the `shard_retired`/reset on the aux tag) is a **separable fix** in `kiseki-raft`'s aux-dispatch / addr resolution; LeaderSink does not *depend* on it for steady-state correctness (steady state uses the new `IntentForward` tag, a fresh code path), and recovery must have it fixed — flagged as a hard pre-req for R5, not a design dependency. *Note for the implementer: the new `IntentForward` RPC and the recovery gather share the same aux-listener mechanism, so fix the transport reset once and both benefit.*
- **B2 (design — min_acks=2 vs 6-voter majority=4 ⇒ intent on 2 never majority-reported).** Eliminated by construction: there is **no watermark and no majority report** in steady state. Incorporation does not require *any* peer to report the intent — the leader incorporates from intents *routed to it* and, at election, from a *majority union* (which does intersect a 2-of-6 durability quorum — §6). The "2 vs 4" mismatch had teeth only because the watermark needed a majority to *see* the intent before draining; that requirement is gone.
- **B3 (design — exclusive watermark seq < W ⇒ latest/only pending never drains when idle).** Eliminated: no watermark, no exclusive bound. The leader drains its *entire* pending store each tick (ascending, floor-gated). The single newest/only pending intent is incorporated on the next tick. Idle shards converge.
- **B4 (design, latent — committer drains only LOCAL store; intent fanned to a subset excluding the leader is incorporated by no one).** Eliminated by construction: every acked intent is **routed to the leader** (steady-state `IntentForward`) and the leader is the **sole** incorporator draining its **own** store. An intent fanned to a follower subset that excluded the leader still reaches the leader via `IntentForward` (step 5), independent of which voters got the `INTENT_PUT` fan copy. Backstop: if the forward is lost and the leader crashes before any forward arrives, the *next* leader's `recover_pending` majority-gather picks it up (it is `min_acks`-durable somewhere in the majority — §6). So B4's "incorporated by no one" is impossible: the steady path routes to the one incorporator, and the recovery path re-derives the complete set.

---

### (5) Recovery (R5) — election handoff, no loss, no double-incorporation, no rollback

On openraft signalling this node is the new leader for the shard (the existing per-shard leadership-change hook):

1. **Fence first.** Do not incorporate until openraft confirms this node is the established leader at the current term (openraft's own commit-of-blank-entry-at-new-term). A deposed old leader's in-flight `IncorporateIntent` appends are rejected by openraft term/leadership checks — this is the split-brain fence (no two leaders append). *(B1 transport fix is a pre-req for the gather in step 2; the fence itself is openraft-native.)*
2. **Gather (O2).** Call `ShardCommitter::recover(peer_pending)` → `recover_pending(views, cluster_size)`: union the *local* pending store with a **majority** of peers' pending stores (via `INTENT_GATHER_PENDING`). Refuse (`InsufficientQuorum`) and retry if fewer than `majority(cluster_size)` distinct nodes answer — never resume on a sub-majority gather (could miss an acked intent). `restore_into` loads the union into the new leader's local store (idempotent on perspective-seq).
3. **Read the floor.** Seed the committer's `max_incorporated_seq` from the *inherited Raft log's* recorded floor (`OpenRaftLogStore::max_incorporated_seq`, already wired in `RaftLogIncorporationSink::new`). This is the state the new leader inherits — everything at/below it is already applied and visible.
4. **Resume incorporation.** Run the normal leader committer loop: drain the local store ascending, **skip any `perspective_seq <= floor`** (F-2: no double-incorporation — an intent the previous leader already appended is dropped, not re-applied), append the rest as `IncorporateIntent`.

**No double-incorporation (F-2).** The floor is in the *replicated, snapshot-included* state machine (`max_incorporated_seq`, lines 76-78 + 569 + 645 + 700). A recovered intent already in the log has `seq <= floor` → skipped. Idempotency-key dedup is the second layer for the rare case of a re-ingressed retry with a *higher* seq but the same key.

**No rollback of visible state (R5/I-L3).** Applied deltas are immutable. A recovered intent that is *newer* (higher perspective-seq) than the current binding for a name correctly supersedes it — that is LWW, the intended behavior, not a rollback. A recovered intent *older* than an already-applied write for the same name: either it is `<= floor` (skipped), or — the subtle case — it is `> floor` but a *higher*-seq write for the same name already applied. Here the older write *does* get appended (it is above the floor), the state machine appends it (tip++), but on the read path it **loses LWW**: the name binding already points at the higher-seq composition. Because compaction resolves by Raft sequence and the *binding* is overwrite-replace by apply order, we must ensure the older recovered write does not clobber a newer binding. **This is handled by appending in ascending perspective-seq order and having the hydrator/`name_insert` apply newer-wins**: since the leader drains ascending, and a recovered older intent is appended *after* the already-applied newer one only if it arrived late — at which point its `IncorporateIntent` carries the older perspective_seq and the state-machine/hydrator must treat a name-bind as conditional on perspective-seq. **Decision:** carry `perspective_seq` into the Create/Update delta payload and make `name_insert` / hydrator `bind_name` **LWW-guarded by perspective-seq** (bind only if incoming seq ≥ stored seq for that name). This is the one read-path hardening LWW-only requires; it is local, deterministic, replicated identically, and closes the late-lower-seq rebind hole without any watermark. *(Flagged for the adversary: this per-name seq guard is the load-bearing replacement for the watermark's rollback-prevention job.)*

---

### (6) min_acks vs voters decision + the O2 overlap math

**Decision: keep `min_acks` divorced from the Raft voter-majority (min_acks=2 on a 6-voter shard is allowed). LeaderSink does NOT require min_acks alignment.** This preserves the full perf win (cheap 2-replica ack on a wide shard).

Why it is still complete — the O2 overlap is on the **recovery gather**, not on a watermark:

- An acked intent is `min_acks`-durable: it lives on **≥ `min_acks`** of the shard's RF replicas. With min_acks=2 on a 6-voter shard, it lives on ≥ 2 of 6.
- The new leader's recovery gather requires a **majority of the membership**: `majority(6) = 4`. It unions the pending stores of ≥ 4 of 6 distinct nodes.
- **Intersection:** any 2-subset and any 4-subset of a 6-set must overlap, because `2 + 4 = 6 ≤ 6` is the *boundary* — two disjoint subsets of sizes 2 and 4 would need 6 distinct nodes, which is exactly the whole set, i.e. they partition it with **zero** overlap. **So 2-of-6 vs 4-of-6 does NOT guarantee intersection.** This is the real subtlety the analyst's tension (d) points at, and it is why I do **not** rely on a bare majority gather alone for min_acks=2 on RF=6.

**Resolution — the gather must intersect the *durability* quorum, so it gathers from `RF − min_acks + 1`, not bare majority.** The completeness condition is: `gather_size + min_acks > RF` ⇒ `gather_size > RF − min_acks` ⇒ `gather_size ≥ RF − min_acks + 1`. For RF=6, min_acks=2: `gather_size ≥ 5`. For RF=3, min_acks=2 (the common case): `gather_size ≥ 2 = majority(3)` — bare majority suffices, matching the existing `recover_pending` tests. **So `recover_pending` must take its threshold as `RF − min_acks + 1` (clamped to ≥ majority for the openraft read-quorum), not `majority(cluster_size)`.** The math:

```
acked intent durable on a set D, |D| = min_acks
recovery gathers a set G,         |G| = RF - min_acks + 1
|D ∩ G| ≥ |D| + |G| - RF = min_acks + (RF - min_acks + 1) - RF = 1  > 0   ✓
```

So at least one gathered store holds every acked intent. For RF=6/min_acks=2 that is a 5-of-6 gather; for RF=3/min_acks=2 it is 2-of-3. **This is a correctness fix to the existing `recover_pending` quorum guard** (it currently uses `majority(cluster_size)`, which is *unsafe* for RF=6/min_acks=2 — a 4-of-6 gather can miss a 2-of-6 acked intent). Flagged as a required change. The tradeoff is owned: recovery (rare, once per election) must reach `RF − min_acks + 1` nodes; if it cannot, it refuses and retries (liveness during a wide-shard double-fault is degraded, but **safety is never traded** — we never resume on an under-gathered set). Steady-state ack stays cheap at min_acks=2.

**Alternative considered + rejected:** align `min_acks = majority(RF)` (4 on RF=6). Makes bare-majority gather safe but erodes the perf win (4-replica ack on every write). Rejected — the recovery-threshold fix gets the same safety at zero steady-state cost.

---

### (7) Failure modes deliberately handled

- **Old-leader-still-incorporating (fencing).** Only the Raft leader at the current term can append; openraft rejects a stale leader's `client_write`/append with a `ForwardToLeader`/term error. The new leader fences (step 5.1) by waiting for established leadership before incorporating. Two leaders cannot both append ⇒ no split-brain, no divergent order.
- **Partial fan (intent on a follower subset excluding the leader — B4).** Steady-state `IntentForward` routes it to the leader regardless of which voters got the `INTENT_PUT` copy. If the forward is lost, recovery's `RF − min_acks + 1` gather re-derives it. Incorporated-by-no-one is impossible.
- **Slow / silent minority.** Steady state does not consult peers at all (no watermark) ⇒ a slow minority cannot stall incorporation (B2/F-1 gone structurally). Recovery tolerates up to `min_acks − 1` silent nodes (still reaches `RF − min_acks + 1` of the remainder); beyond that it refuses + retries (safe).
- **Idle shard.** One pending intent → forwarded immediately → incorporated on the next leader tick. No watermark to stall (B3 gone).
- **node-X (ingress) crashes after ack but before IntentForward.** The intent is `min_acks`-durable (that is *why* the ack was safe). The leader either already received a forward (from a *different* replica that also holds it — the forward can be sent by any holder, or re-driven), or, failing that, the **next election's** `recover_pending` gather (`RF − min_acks + 1`) picks it up because it intersects the `min_acks` durability set. So an ingress crash post-ack never loses the write (R1). *(Design choice: `IntentForward` is driven by the ingress node as the common case, but the recovery gather is the *guarantee* — the forward is an optimization for steady-state latency, not the no-loss mechanism.)*
- **Leader crashes mid-drain (incorporated some, not all).** F-2 floor: the new leader skips the incorporated prefix (`seq <= floor`), re-incorporates the rest from the recovered union. No double-incorporation, no gap.
- **Clock skew picking wrong LWW winner (O1).** Inherited from ADR-047 §1/O1: max-skew bound B, clamp + alert beyond it, fence an NTP-failed node from ingress. The per-name seq-guard (§5) makes the resolution deterministic-and-identical on every replica even if a human would call the winner "wrong" — that is the documented HLC tradeoff, not a new hole.

---

### (8) Reused vs replaced

**Reused (genuinely fits — not sunk-cost):**
- `crate::intent` — `PerspectiveSeq`, `WriteIntent`, `IntentStore` trait, `InMemIntentStore`, **`FjallIntentStore`** (durable, format-versioned, idem-deduped). Unchanged.
- `put_intent_and_fan` (`raft_shard_store.rs:395`) — the `min_acks` quorum-write + fast-ack producer. Unchanged (this is the ack-path that *works*).
- `LogCommand::IncorporateIntent` + state-machine apply (`state_machine.rs:418`) — **modified**: the floor must *gate the append* (skip if `seq <= max_incorporated_seq`), not merely record the max. Add an `idempotency_key` dedup index to the state machine (snapshot-included) per O3/F-2.
- `max_incorporated_seq` (state-machine field + snapshot round-trip) — unchanged; now does real work as the F-2 skip floor.
- `RaftLogIncorporationSink` + `IntentLogAppender` (`raft_intent_sink.rs`) — unchanged; this is the leader's single-writer sink.
- `recover_pending` / `restore_into` / `ShardCommitter::recover` — reused for election recovery, with **one correctness fix**: threshold becomes `RF − min_acks + 1` (clamped ≥ openraft read-majority), not `majority(cluster_size)` (§6).
- `INTENT_GATHER_PENDING` aux RPC + `TransportIntentGatherer::gather_pending` — reused **for recovery only**.
- The hydrator + name index — reused; **one hardening**: `bind_name`/`name_insert` become **LWW-guarded by perspective-seq** (bind only if incoming ≥ stored), and the Create/Update delta payload carries `perspective_seq` (§5). This is the watermark's rollback-prevention job, relocated to a local per-name guard.

**Replaced / deleted:**
- `compute_stability_watermark` + `Watermark` + `Bound` (`intent_committer.rs:32-124`) — **deleted.** No watermark in LWW-only.
- `PeerIntentGatherer::gather_next_pending_seqs` + `IntentStore::next_pending_seq` + `INTENT_NEXT_PENDING` aux tag — **deleted** (steady-state peer-gather gone; this is most of B1's surface).
- `Committer::run`'s watermark logic + `ShardCommitter::tick(peer_reports)` (`shard_committer.rs:131`) — **replaced** by a leader-only `drain_local()` that does: read floor → take pending `> floor` → sort ascending → `incorporate` → `prune`. No peer reports, no `next_pendings` slice.
- The every-node committer model (tension b) — **replaced** by leader-only spawn: the per-shard committer loop runs **only on the node that is the shard's Raft leader**, started/stopped by the leadership-change hook (the same hook that triggers recovery). Followers run no committer.

**New (small):**
- `INTENT_FORWARD` aux tag + `IntentForward` RPC (ingress → leader): carries the same `WireIntent` as `INTENT_PUT`; the leader records it in its `IntentStore` (idempotent on perspective-seq + idem-key). Fire-and-forget with bounded retry, off the ack path. Reuses the existing aux-listener + `WireIntent` codec — minimal new surface.
- Leader-only committer spawn/teardown on the leadership-change hook (extends the existing `committers` map + `shutdown` in `raft_shard_store.rs`).

---

## PART 3 — ADVERSARY REVIEW

Stance: every load-bearing claim verified against code. The core direction
(single-incorporator-on-leader, drop the watermark, LWW-only) is sound — but
several of the architect's "reused / already exists" claims are **false against
the current tree**, and one of those (the per-name seq guard) is the load-bearing
replacement for the deleted watermark. The design is not implementable as written:
it lists as "reused" four mechanisms that do not exist (idempotency dedup index,
state-machine append-gating, the per-name perspective-seq guard, and — most
seriously — *any production wiring that calls recovery on an election*). The
no-loss guarantee R1/R5 is currently **vapor**: `ShardCommitter::recover` is
invoked only in tests (`grep -rn "\.recover(" crates --include="*.rs"` →
shard_committer.rs tests only). That makes the architect's "the forward is an
optimization, recovery is the guarantee" framing a guarantee resting on unwired
code.

Findings below, severity-ordered.

---

### Finding A — Compaction is NOT the LWW resolver for named objects; the name index is. The architect's "load-bearing fact" is a red herring, and the real resolver has no seq guard.

- **Severity:** Critical (it invalidates the §(1) correctness argument and mis-locates the one fix that matters)
- **Category:** Correctness > semantic drift / specification compliance
- **Location:** `compaction_worker.rs:119`; `composition.rs:1402` (`composition_hash_key`); `composition.rs:1024` (`create_with_name` → `Uuid::new_v4()`); `persistent/storage.rs:403` (`name_insert`, overwrite-replace); `mem_gateway.rs:2124` (`lookup_object_by_name`).
- **Description:** The architect's §(1)/§(2) argument hinges on "compaction collapses LWW by the local Raft sequence (`compaction_worker.rs:119`)" and concludes the surviving same-name write is whichever has the higher Raft index. **That is false for named S3/native objects.** `compact_deltas` groups by `delta.header.hashed_key` (line 102/106). `hashed_key = composition_hash_key(ns, comp_id) = uuid_v5(ns, comp_id)` — derived from **comp_id**, and `create_with_name` mints a **fresh random `comp_id` (`Uuid::new_v4()`) per write**. So two writes to the same NAME produce two DIFFERENT comp_ids → two DIFFERENT hashed_keys → **two separate compaction groups**. Compaction never makes them compete; both versions are retained. The actual LWW winner for a name is decided **entirely** by the name index `(ns,name)→comp_id` binding, which is pure **overwrite-replace ordered by hydrator apply order = Raft log order** (`name_insert` storage.rs:403-428; hydrator `apply_hydration_batch` storage.rs:503-519; `stage_create` re-binds unconditionally even on the comp-already-exists path, hydrator.rs:534-538). There is **no perspective-seq anywhere in this path** today.
- **Evidence:** `composition_hash_key` body (uuid_v5 over comp_id); `create_with_name` line 1024 `CompositionId(uuid::Uuid::new_v4())`; `compact_deltas` groups by `header.hashed_key`; `name_insert` is overwrite-replace with no seq compare; `decode_composition_create_payload_named` carries no seq field. The reordering attack the analyst posed (node-2's newer S2 IntentForward arrives first, node-1's older S1 arrives later → leader appends S2 then S1 → S1 has the higher Raft index → S1 binds the name LAST → **acked newer write S2 silently lost to older S1**) is **real and reproducible** under LeaderSink-as-written, because the leader drain is the only ordering and a late-arriving forward jumps the queue. The architect's own §(5) "Decision" admits this and proposes the per-name seq guard — but the *justification* in §(1)/§(2) (compaction by Raft seq) is wrong, which matters because it makes the guard look optional ("compaction already handles it") when it is **mandatory and the sole** resolver.
- **Suggested resolution:** Rewrite §(1)/§(2) to state plainly: for named objects, compaction does not resolve same-name LWW (distinct comp_ids → distinct hashed_keys); the name-index bind order is the sole resolver. Therefore the per-name perspective-seq guard (Finding B) is **non-negotiable and load-bearing**, not a hardening footnote. Also confirm whether ANY consumer keys same-name versions under a shared hashed_key (versioning layer?) — if not, the compaction-LWW claim should be struck entirely from the rationale.

---

### Finding B — The per-name perspective-seq guard (the watermark's replacement) does not exist, is under-specified, and must be applied in THREE places, not one. Get any wrong and the Finding-A reorder loses an acked write.

- **Severity:** Critical
- **Category:** Correctness > concurrency / illegal-state-prevention
- **Location:** design §(5) "Decision" + §(8) "one hardening"; code: `hydrator.rs:105` (`bind_name`, no seq), `hydrator.rs:534-538` (`stage_create` re-bind, no seq), `persistent/storage.rs:403` + `503-519` (`name_insert` / `apply_hydration_batch`, no seq), `composition.rs:188` (`encode_composition_create_payload_named`, no seq field), `mem_gateway.rs:2533-2547` (ingress local `create_with_name`, no seq).
- **Description:** The design says "make `name_insert`/`bind_name` LWW-guarded by perspective-seq (bind only if incoming ≥ stored)" and calls it "local, deterministic, replicated identically." Verified: **none of this machinery exists.** To make it correct the guard must hold at *every* place a name is bound, and the stored per-name seq must be durable and snapshot/replicated-consistently:
  1. **Hydrator in-batch** (`Staging::bind_name`, hydrator.rs:105): within one poll a later delta for the same name must win — needs an in-batch `(ns,name)→best_seq` compare, not blind `push`.
  2. **Persistent apply** (`apply_hydration_batch` name_inserts loop, storage.rs:503 AND fjall.rs:493): cross-batch — must read the stored per-name seq and skip if incoming `< stored`. This requires a **new durable column** `(ns,name)→perspective_seq` in BOTH the in-mem and fjall stores, snapshot/restart-consistent.
  3. **The Create/Update delta payload must carry `perspective_seq`** (`encode/decode_composition_create_payload_named`) — a **wire-format version bump** (the doc hand-waves this as "carry perspective_seq into the payload"). Today the delta's HLC is `physical_ms = log_index` (state_machine.rs:279), NOT the perspective_seq, so the seq is *not even recoverable* from the delta header — it MUST be added to the payload explicitly.
  4. **`stage_create`'s comp-already-exists re-bind** (hydrator.rs:534-538) re-binds unconditionally — must also respect the guard, or a replayed older Create clobbers a newer binding.
- **Evidence:** `bind_name` is a bare `Vec::push`; `name_insert` overwrite-replaces with no seq; `HydrationBatch.name_inserts: Vec<(NamespaceId, String, CompositionId)>` has no seq field; the create payload decode returns `(comp_id, ns, size, name, lens)` — no seq.
- **Suggested resolution:** Specify the guard fully: (a) extend the create/update payload to v-next with `perspective_seq`; (b) add a durable `(ns,name)→perspective_seq` map to `CompositionStorage` (both backends), snapshot-included; (c) guard all four bind sites with `incoming_seq >= stored_seq`; (d) **define the tie at `==`** (idempotent replay of the same seq must be a no-op-or-equal, never a flip) — note PerspectiveSeq is globally unique by node_id tiebreak, so a true `==` across different writes is impossible, but a replay of the SAME write must compare equal and bind to the same comp_id; (e) a deletion's unbind must ALSO be seq-guarded (a late older Delete must not unbind a newer Create's name — `stage_delete` hydrator.rs:620 currently unbinds unconditionally). Add a BDD scenario that drives the exact analyst reorder (S2 forwarded-first, S1 forwarded-later) on a real multi-node `ClusterHarness` and asserts GET-by-name returns S2.

---

### Finding C — R1/R5 no-loss is unwired: recovery is never called on an election in production. The architect's "recovery is the guarantee, the forward is just an optimization" rests on dead code.

- **Severity:** Critical
- **Category:** Correctness > specification compliance / failure cascades
- **Location:** design §(5) + R1/R5 table rows + §(8) "Reused: `recover_pending`/`restore_into`/`ShardCommitter::recover` — reused for election recovery"; code: `ShardCommitter::recover` (shard_committer.rs:198) — callers are tests only.
- **Description:** `grep -rn "\.recover(" crates --include="*.rs"` returns only shard_committer.rs test lines (458/470/483/511). There is **no leadership-change hook and no production call site** that runs `ShardCommitter::recover` (or `recover_pending`) when a node becomes shard leader. `run_committer_loop` explicitly documents "Election-triggered recovery is NOT run here" (shard_committer.rs:246-249). So the entire R1/R5 no-loss spine — which the architect repeatedly leans on as "the guarantee" while demoting the forward to "an optimization" — **does not run at all today.** The committer is also spawned on **every node at `create_shard` time** (raft_shard_store.rs:643-644), NOT leader-only, and there is no `become_leader` signal wired to start/stop it or to trigger recovery. The design lists both the leader-only spawn AND the recovery-on-election as "reused / extends existing hook," but the hook does not exist.
- **Evidence:** No production caller of `.recover(`; `spawn_committer` called unconditionally in `create_shard`; `run_committer_loop` doc line 246-249; no `metrics_changed`/`ServerState::Leader` watcher anywhere in kiseki-log or kiseki-server tied to committer lifecycle (only `storage_admin.rs:1609` reads `current_leader()` for an unrelated admin path).
- **Suggested resolution:** Reclassify recovery wiring + leader-only committer lifecycle as **NEW (large), not reused.** Specify the leadership signal source (openraft `Raft::metrics()` watch on `current_leader`/`ServerState`), the per-shard start/stop, and the fence-before-recover-before-resume sequence. Until this is wired, R1/R5 are **unsatisfied** and the design's own claim ("(a) forward, (b) recovery; (b) IS the no-loss guarantee") is false. This is the single biggest gap between the doc and the tree.

---

### Finding D — Permanent invisibility without an election (a new B4-class hole). Ingress crashes post-ack/pre-forward, leader stable, leader not in the min_acks set → the intent is never incorporated and never recovered.

- **Severity:** High
- **Category:** Correctness > failure cascades / liveness (violates R4 "no permanent invisibility")
- **Location:** design §(7) "node-X crashes after ack but before IntentForward" + R4 row; code: `put_intent_and_fan` (raft_shard_store.rs:395) fans to voter peers, leader not guaranteed in the min_acks subset; recovery is election-only (Finding C).
- **Description:** The acked intent lives on `min_acks` voters (e.g. 2 of 6). The forward never fired (ingress crashed). If the leader is **not** one of those 2, the leader's store never receives it, and — because recovery only runs on an **election** — it sits invisible **indefinitely** as long as the leader stays up. The architect's §(7) answer ("the next election's recover_pending picks it up") is only true *if an election ever happens*. A stable healthy leader can run for days. This is structurally **B4 reborn**: "incorporated by no one" until an unrelated election. The current (broken) every-node committer at least had every node draining its own store — so even a leader-excluded intent would be drained by *some* node's committer (then lose the append race, but the watermark/gather machinery would surface it). LeaderSink removes that, making the no-election orphan strictly worse on this axis.
- **Evidence:** Forward is fire-and-forget from ingress only (§(7) "driven by the ingress node as the common case"); no peer re-drives a forward for a crashed ingress; recovery election-only (Finding C / shard_committer.rs:246). The fan in `put_intent_and_fan` stops at `min_acks` (line 464-468) and does not prefer the leader.
- **Suggested resolution:** Make the forward not depend on the (possibly dead) ingress node. Options: (a) **fan-includes-leader** — `put_intent_and_fan` must count the leader among the min_acks targets (preferred: the leader is then always a holder and its own drain incorporates it, no forward needed); or (b) a **periodic leader-pull**: the stable leader periodically gathers `next_pending` from voters and pulls anything below its floor (turns recovery into a steady-state safety net, not election-only); or (c) any voter holding an un-incorporated intent past a deadline re-drives the forward. (a) is the cleanest and also closes Finding F. Without one of these, R4 is violated.

---

### Finding E — Idempotency dedup index does not exist; idempotency_key is dropped at the sink boundary. F-2/O3 exactly-once is unprovable as designed.

- **Severity:** High
- **Category:** Correctness > idempotency (F-2, O3)
- **Location:** design R3 row + §(8) "Add an idempotency_key dedup index to the state machine (snapshot-included)"; code: `LogCommand::IncorporateIntent` (state_machine.rs:418, no idempotency_key field), `append_intent` (openraft_store.rs:557-568, does not pass idempotency_key), `RaftLogIncorporationSink::incorporate` (raft_intent_sink.rs:138-149, passes only `intent.append` + `perspective_seq`, drops `intent.idempotency_key`).
- **Description:** The R3 row claims "two layers, both in the replicated state machine … (ii) client idempotency_key dedup index, snapshot-included." **Neither the index nor the plumbing exists.** `IncorporateIntent` has no idempotency_key field; `append_intent` never carries it; the sink throws it away. So the *only* dedup is the `max_incorporated_seq` floor — which keys on **perspective_seq**, not on the client key. The floor is sufficient for the "same intent re-gathered by recovery" case (same seq → skipped IF the append-gate is added, Finding G), but it is **NOT** sufficient for the case the architect explicitly cites as why the second layer exists: a **re-ingressed client retry with a NEW perspective_seq but the same idempotency_key** (a client retries a PUT; the gateway mints a fresh seq). That produces two distinct seqs, both above the floor, both appended → **two compositions, two name binds** (and the later seq wins LWW, so it's not a data-corruption but it IS a duplicate-incorporation / refcount + chunk-state inflation, and violates O3 at-most-once). The architect's own ingress code already notes this gap (`mem_gateway.rs:2684` "TODO(ADR-047 §5/O3): a stable 16-byte derivation would give exactly-once").
- **Evidence:** `IncorporateIntent` variant has fields tenant/op/hashed_key/chunk_refs/payload/has_inline_data/new_chunks/perspective_seq — no idempotency_key; sink `incorporate` clones `intent.append` only.
- **Suggested resolution:** If O3 exactly-once is in scope, this is NEW work: thread `idempotency_key` into `IncorporateIntent`, add a replicated, snapshot-included `idempotency_key → seq` dedup map in the state machine, and gate the append on it. If O3 is OUT of scope for this phase (at-least-once accepted, clients dedup via LWW), then **strike the "two layers" claim** from R3 and document at-least-once + the LWW-collapse as the contract, with the refcount/chunk-state double-count called out as a known cost. Do not ship the doc claiming a dedup index that isn't built.

---

### Finding F — Follower/ingress intent stores grow without bound: no prune signal reaches non-leader holders.

- **Severity:** High
- **Category:** Robustness > resource exhaustion
- **Location:** design §(8) "Followers run no committer"; code: `prune` called only in `Committer::run` (intent_committer.rs:289), which runs only on the incorporator.
- **Description:** `put_intent_and_fan` writes a durable copy on the ingress node and fans durable copies to `min_acks − 1` voter peers (raft_shard_store.rs:421-468). Under LeaderSink only the **leader** runs a committer and prunes **its own** store. The min_acks copies on the other voters (which are NOT the leader in the common case) are **never pruned** — there is no "leader incorporated seq S, you may drop ≤ S" signal. The FjallIntentStore on every follower therefore grows monotonically for the life of the shard. Over a perf run (35k–125k PUT/s) this is GiB/minute of durable intent records that never GC. The current every-node committer at least pruned each node's local store. This is a hard production blocker for any sustained workload, independent of the correctness findings.
- **Evidence:** `prune` sole caller is `Committer::run`; followers spawn no committer in the design; no prune-broadcast / floor-gossip mechanism in `intent_sync.rs` (it only serves `next_pending` and `gather_pending` reads).
- **Suggested resolution:** Add a prune signal: the leader periodically broadcasts its `max_incorporated_seq` (or piggybacks it on the existing aux gather) and each follower prunes `≤ floor` from its local store. This must be **safe against the Finding-D orphan**: a follower must NOT prune an intent the leader has not actually incorporated — so the broadcast floor must be the *replicated* `max_incorporated_seq` the follower can read from its OWN applied Raft state (already available — followers apply `IncorporateIntent` too), not a leader-asserted value. In fact a follower can self-prune by comparing its local intent seqs against its own applied `max_incorporated_seq` — no broadcast needed. Specify this; it is missing entirely.

---

### Finding G — The append-gate (skip-if-`seq ≤ floor`) is genuinely missing AND the floor is read from a possibly-stale leader cache, not the applied state. Double-incorporation window on the leader itself.

- **Severity:** High
- **Category:** Correctness > idempotency / concurrency
- **Location:** design §(2) note + §(8) "modified: the floor must gate the append"; code: state_machine.rs:418-443 (appends unconditionally, only records max); `Committer::run` floor read from `sink.max_incorporated_seq()` (intent_committer.rs:254), which is the sink's **cached** `max_incorporated` (raft_intent_sink.rs:88, advanced locally in `incorporate` line 146).
- **Description:** Two distinct correctness gaps the doc conflates:
  1. The state-machine append-gate the architect requires (skip if `perspective_seq ≤ max_incorporated_seq`) **does not exist** — `IncorporateIntent` appends unconditionally (state_machine.rs:429) then bumps the max (438-441). Confirmed not yet present. Until added, recovery WILL double-incorporate (a re-gathered intent already in the log is re-appended). The doc correctly identifies this as a needed change but lists it under "modified (reused)" — it is a real behavioral change with its own tests.
  2. **More subtly:** the committer's floor (`Committer::run` line 254) comes from the sink's **in-memory cache** (`RaftLogIncorporationSink.max_incorporated`), advanced optimistically in `incorporate` (line 146) *before* the apply is confirmed replicated, and seeded once at construction (line 100). On a leader that just recovered, the cache is seeded from `appender.max_incorporated_seq()` (the applied log) — good — but if an append's `client_write` returns Ok yet the apply is later overwritten by a higher-term leader's log (the deposed-leader-with-in-flight-write case), the cache is ahead of the truth. The append-gate in the state machine (gap 1) is the real defense; the cache is only an optimization and must never be trusted as the sole F-2 floor. The doc's §(2) note treats the floor-skip as belt-and-suspenders but the *committer-side* filter (line 259-265) currently does the floor work using the cache — that filter must NOT be the only gate.
- **Evidence:** state_machine.rs:429 unconditional `append_delta_inner`; sink cache advance at raft_intent_sink.rs:146; committer filter at intent_committer.rs:254-273.
- **Suggested resolution:** Make the **state-machine append-gate** authoritative (it is replicated and identical on every replica): in `IncorporateIntent` apply, `if perspective_seq <= max_incorporated_seq { return Appended(self.tip) /* no-op */ }`. Keep the committer-side cache filter only as a perf pre-filter. Add a test: append seq S twice (simulating recovery re-incorporation) → exactly one delta in the log. Note this changes the `LogResponse::Appended(tip)` contract for the skip case (returns the unchanged tip) — define it.

---

### Finding H — Recovery overlap math is correct, BUT the liveness cost is understated and the cluster_size/RF wiring is ambiguous.

- **Severity:** Medium
- **Category:** Correctness (math: confirmed) + Robustness (liveness: understated)
- **Location:** design §(6); code: `recover_pending` (intent_committer.rs:341, uses `majority(cluster_size)`), `majority` (intent_committer.rs:32).
- **Description:** (a) **The overlap math is correct.** `|D ∩ G| ≥ |D| + |G| − RF = min_acks + (RF−min_acks+1) − RF = 1 > 0`. And the existing guard IS unsafe for RF=6/min_acks=2: `majority(6)=4`, and a 2-subset and 4-subset of a 6-set CAN be disjoint (2+4=6), so a 4-of-6 gather can miss a 2-of-6 acked intent — confirmed bug in the shipped `recover_pending`. The fix `gather ≥ RF − min_acks + 1` (=5 for RF6/m2) is right. (b) **Liveness is worse than the architect frames it.** A 5-of-6 gather means recovery STALLS with **2 nodes down** — and a leader election is often *triggered by* a node going down, so the very moment you need recovery is when you're most likely below 5-of-6. The synchronous Raft baseline tolerates minority loss (commits with 4-of-6) and serves reads throughout; LeaderSink-recovery refusing at 2-down means a new leader **cannot resume incorporation** → the shard accepts no new visible writes until ≥5 come back. That is a real availability regression on wide shards under the common double-fault, and the doc's "(liveness during a wide-shard double-fault is degraded)" parenthetical undersells it. (c) **cluster_size vs RF ambiguity:** `recover_pending`/the committer take `cluster_size = self.peers.len()` (raft_shard_store.rs:665), the configured voter count. The fan targets `resolve_voter_peers` (voter set). So RF = voter count = cluster_size here, and `RF − min_acks + 1` is coherent — BUT the doc says "clamped to ≥ majority for the openraft read-quorum"; that clamp is a no-op (RF−min_acks+1 ≥ majority always when min_acks ≤ majority) and adds confusion. State the threshold as exactly `cluster_size − min_acks + 1`.
- **Evidence:** `majority` body (cluster_size/2+1); `recover_pending` guard line 341-344; `spawn_committer` cluster_size = peers.len() line 665.
- **Suggested resolution:** Land the threshold fix as `cluster_size − min_acks + 1` (drop the misleading clamp note). EXPLICITLY document the liveness tradeoff as a first-class consequence, not a parenthetical: "on RF=6/min_acks=2, recovery (hence resumption of new-write visibility on the new leader) requires 5 of 6 voters reachable; a 2-node-down election leaves the shard unable to incorporate until a 5th returns. Already-visible reads continue." Confirm this is acceptable vs. the alternative (align min_acks=majority, which the architect rejected for perf). Consider: a smaller min_acks=2 on a *narrow* RF=3 shard needs only 2-of-3 — fine. The pain is specifically wide shards with tiny min_acks; flag that combination in ops docs.

---

### Finding I — Read-your-writes divergence: the ingress node's local optimistic name binding can permanently disagree with the incorporated truth.

- **Severity:** Medium
- **Category:** Correctness > concurrency (RYW vs LWW consistency, I-CS2)
- **Location:** design R7-relevant (RYW claim in §(2) step notes); code: `mem_gateway.rs:2533-2547` (local `create_with_name` installs `(ns,name)→comp_id_local`, no rollback), `mem_gateway.rs:2700-2702` ("Do NOT roll back the composition").
- **Description:** On ingress, node-X creates a **local** composition with comp_id_X and binds `(ns,name)→comp_id_X` immediately (RYW), and never rolls back. Concurrently node-Y ingests the same name as comp_id_Y with a HIGHER perspective_seq. The leader incorporates both; with the Finding-B guard installed, the authoritative replicated binding becomes `(ns,name)→comp_id_Y` (higher seq wins). Now node-X's **local** name index still says comp_id_X (its optimistic bind was unconditional and is never reconciled by the hydrator, because the hydrator's seq-guard will *correctly refuse* to overwrite... wait — node-X's local store has comp_id_X bound with X's seq; the hydrator applies Y's Create with a higher seq → guard says higher wins → rebinds to comp_id_Y). So WITH the guard, node-X converges. **But without the guard, or if the local optimistic bind doesn't record a seq the hydrator can compare against, node-X serves comp_id_X forever** (its own write) while every other node serves comp_id_Y. That is a per-node split: GET on node-X returns the loser, GET elsewhere returns the winner — a silent RYW-vs-global divergence. This is entirely contingent on Finding B's guard being applied to the **local ingress bind path too** (mem_gateway create_with_name), which the design does not mention — §(5) only mentions the hydrator/name_insert.
- **Evidence:** ingress `create_with_name` has no seq; comp not rolled back (line 2700); the guard in §(5) is scoped to "hydrator bind_name / name_insert," omitting the ingress optimistic bind.
- **Suggested resolution:** The local optimistic bind on ingress MUST record its own perspective_seq in the same `(ns,name)→seq` durable map, so the hydrator's seq-guard correctly arbitrates between the local optimistic bind and any incoming higher-seq replicated bind. Add to Finding B's site list: the ingress `create_with_name` path. Add a 2-node concurrent-same-name BDD asserting all nodes (including both ingress nodes) converge to the highest-seq comp_id.

---

### Finding J — Single-incorporator liveness: leader committer wedge stalls incorporation cluster-wide, and the `block_on`/`block_in_place` nesting is a known foot-gun with no watchdog.

- **Severity:** Medium
- **Category:** Robustness > failure cascades / observability
- **Location:** design §(6)(b) tension + R4; code: `spawn_committer` dedicated thread + `block_on_maybe_in_place` (raft_intent_sink.rs:119-125), `run_committer_loop` swallows all errors (shard_committer.rs:277-285).
- **Description:** With LeaderSink, exactly one thread (the leader's per-shard committer) incorporates. If it wedges — the nested `block_on` deadlock the threading-contract comment warns about (raft_intent_sink.rs:64-84), a panic in the thread, or a slow Fjall append — incorporation stops **cluster-wide** for that shard: every acked write piles up invisible (RYW-only) until the thread recovers or an election moves leadership. `run_committer_loop` swallows per-tick errors silently (line 280 `tracing::warn!` then continues), and a **panic** in the committer thread is not caught — `spawn_committer` keeps the JoinHandle but nothing monitors it; a dead committer thread is indistinguishable from an idle one. There is no heartbeat/metric for "committer alive + draining," no auto-restart, and no alarm on growing pending depth. The broken every-node design was wasteful but had N committers (one per node) — losing one didn't stall the shard. Single-incorporator concentrates that risk.
- **Evidence:** committer thread JoinHandle stored but unmonitored (raft_shard_store.rs:706-715); no liveness metric; `run_committer_loop` error-swallow; the sink's own doc flags the nested-block_on panic risk.
- **Suggested resolution:** Add (a) a committer liveness/heartbeat metric + a `pending_depth` gauge per shard with an alarm; (b) panic-catch + auto-respawn in `spawn_committer` (or tie respawn to the leadership hook); (c) prefer driving the loop off an event-wake (the design's "event-driven on every put_intent_and_fan") with a tick fallback, so a wedge is detectable as "woken but not draining." Treat single-incorporator as requiring an explicit watchdog, not best-effort.

---

### Finding K — Perspective-seq is NOT causally monotone across nodes; clock skew can make an objectively-later write lose LWW (and the loss is now silent + permanent).

- **Severity:** Low (documented HLC tradeoff, but the blast radius changed)
- **Category:** Correctness > semantic drift (O1)
- **Location:** design §(7) "Clock skew picking wrong LWW winner (O1)"; code: `next_perspective_seq` (mem_gateway.rs:639-649), `HybridLogicalClock::tick` (time.rs:62), Ord (time.rs:140-147).
- **Description:** perspective_seq = HLC stamped on the ingress node from its **local wall clock** (`tick(now_physical_ms)`), with node_id tiebreak. Two same-name writes ingressed on different nodes are ordered by (physical_ms, logical, node_id). If node-Y's clock is behind node-X's by > the true inter-write gap, an objectively-later write on node-Y gets a LOWER perspective_seq and **loses** LWW to the earlier write on node-X. This is the standard HLC/skew tradeoff and the architect cites O1's clamp+fence. The relevant adversarial point: under the OLD synchronous path, same-name writes were serialized through the leader's Raft append in arrival order, so wall-clock skew didn't decide the winner. Under LeaderSink, perspective_seq IS the winner-decider (Finding A/B), so **clock skew now directly determines named-object LWW**, and the loss is silent (no error) and permanent (LWW). The HLC tiebreak guarantees a *deterministic* winner (good — no split), but not the *intuitively correct* one. Confirmed: PerspectiveSeq derives Ord from HLC; node_id breaks exact ties so two distinct writes can never share a seq (this part of the architect's §5/Finding-5 claim is TRUE — no HLC collision across nodes).
- **Evidence:** `next_perspective_seq` uses the node-local clock; HLC Ord uses physical_ms first; no cross-node causal merge on the ingress write (the merge rule exists on `HybridLogicalClock::merge` but the ingress path uses `tick`, not a merge against a received clock).
- **Suggested resolution:** Acceptable to document as the inherited O1 tradeoff, BUT make it explicit in the ADR that with LeaderSink, named-object LWW is decided by ingress wall-clock + skew clamp, and that the O1 max-skew bound B is now a **correctness** parameter for named objects, not just an audit nicety. Ensure the skew clamp + NTP-fence is actually wired (verify separately — it was not in scope of this review). Add the bound B to ops docs as a named residual gap.

---

### Finding L — Deposed-leader committer keeps spinning (safe but noisy) until a leadership hook that does not exist tears it down.

- **Severity:** Low
- **Category:** Robustness > observability / wasted work
- **Location:** design §(7) fencing + §(8) leader-only spawn; code: committer spawned per-node unconditionally (raft_shard_store.rs:643), no teardown-on-loss.
- **Description:** Fencing is real — a deposed leader's `IncorporateIntent` `client_write` returns `ForwardToLeader` (openraft_store.rs:570-581), which `run_committer_loop` swallows (no double-append — safe). But because the leader-only spawn/teardown hook does not exist (Finding C), today EVERY node's committer keeps trying to append and failing on followers; under LeaderSink-as-specified a freshly-deposed leader keeps spinning its committer (re-gathering, re-attempting client_write, getting ForwardToLeader) until the (nonexistent) hook stops it. Safe, but burns CPU + log noise and muddies the Finding-J liveness signal (a spinning-but-failing committer looks alive).
- **Evidence:** unconditional spawn; ForwardToLeader mapping; error-swallow in the loop.
- **Suggested resolution:** Subsumed by Finding C's leadership hook: stop the committer promptly on leadership loss. Until then, downgrade the ForwardToLeader-on-append from a warn to a debug + a "not-leader, parking committer" state so it's not mistaken for a wedge.

---

### Findings the analyst's R1–R6 / non-negotiables touch that the design handles correctly (for the record)

- **R6 / I-L5 data-before-metadata:** preserved. `put_intent_and_fan` writes chunks to min_acks before the intent (verified: chunk write precedes intent in the gateway path, mem_gateway.rs:2659 "write_chunk happened above, BEFORE the intent"). LWW-only does not weaken this. OK.
- **POSIX-stays-synchronous scope:** the async path is gated on `req.surface.is_async_ack_eligible()` (mem_gateway.rs:2666) — S3/native only; NFS/FUSE fall to the synchronous emit. LeaderSink does not change that gate. OK, provided the leader-only committer + recovery are ALSO gated off when decoupled_ack is off (they are spawned only under `self.decoupled_ack && intent_store_durable`, raft_shard_store.rs:643 — OK).
- **Split-brain / I-L1 single total order:** the single-writer-is-the-leader claim is sound; openraft term-fencing on `client_write` is real. OK (modulo Finding C wiring).
- **No HLC seq collision across nodes (analyst probe 5):** confirmed — node_id tiebreak makes PerspectiveSeq globally unique; two different intents cannot share a seq. OK.

---

## VERDICT: **REJECT** (as written) → re-submit as ACCEPT-WITH-CHANGES once the doc matches the tree.

The strategic direction is correct and I endorse it: single-incorporator-on-leader, drop the watermark, LWW-by-perspective-seq. Do not reopen that decision. **But the document is not an implementable design** — it repeatedly classifies as "reused / already exists / one small modification" four mechanisms that are absent from the codebase, and one of them (recovery wiring) is the very thing it calls "the no-loss guarantee." Shipping against this doc would reproduce the class of error that already shipped once: a design whose safety rests on code that isn't wired.

**Reframe for re-submission:** split the doc's "Reused vs Replaced vs New" so it is HONEST against the tree, then the same architecture is acceptable. Specifically, reclassify as **NEW (not reused):** (1) leadership-change hook + leader-only committer lifecycle; (2) election-triggered recovery call site (currently test-only); (3) the per-name perspective-seq guard incl. the durable `(ns,name)→seq` map, the payload version bump, and all FOUR bind sites incl. ingress; (4) the state-machine append-gate; (5) the idempotency dedup index (or strike the exactly-once claim); (6) follower intent-store self-prune.

**Must-fix before implementation (blocking):**
1. **Finding C** — wire recovery on election + leader-only committer; until then R1/R5 are unmet. Reclassify as NEW.
2. **Finding B + A** — fully specify the per-name perspective-seq guard at ALL bind sites (in-batch, persistent both backends, stage_create re-bind, stage_delete unbind, AND ingress optimistic bind per Finding I); bump the create/update payload to carry perspective_seq; add the durable per-name seq map. Strike the wrong compaction-LWW justification.
3. **Finding D** — close the no-election orphan: fan-includes-leader (preferred) or a steady-state leader-pull safety net. R4 is otherwise violated.
4. **Finding G** — add the state-machine append-gate as the authoritative F-2 floor; do not rely on the sink cache.
5. **Finding F** — specify follower intent-store self-prune (followers compare local intents against their own applied max_incorporated_seq). Otherwise unbounded growth blocks any sustained run.
6. **Finding E** — either build the idempotency dedup index (thread the key through IncorporateIntent + replicated map) OR explicitly downgrade the contract to at-least-once + document the refcount/chunk-state double-count. Do not claim a dedup index that isn't built.

**Should-fix (non-blocking but document):**
7. **Finding H** — land `cluster_size − min_acks + 1` threshold; document the wide-shard double-fault liveness regression as a first-class tradeoff.
8. **Finding J** — committer watchdog: liveness metric, pending-depth alarm, panic-respawn.
9. **Finding K** — document that named-object LWW is now skew-sensitive; make O1's bound B a named correctness parameter; verify the NTP-fence is wired.

Once findings 1–6 are addressed in the doc (with the honest reuse/new split) and the must-fixes have specified mechanisms + tests, this becomes ACCEPT-WITH-CHANGES and can proceed to implementation. The reorder BDD (analyst's S2-first/S1-later) and the 2-node concurrent-same-name convergence BDD are gate conditions on a real multi-node ClusterHarness, not in-memory mocks (the shared-store unit tests false-pass replication — per the project's own #127 note).

---

## PART 4 — SYNTHESIS / DECISION

**Converged design: LeaderSink — single-incorporator-on-the-leader, no stability watermark, per-key LWW by perspective-seq.** The adversary endorsed this direction; do NOT reopen it. R2(a) is resolved: full per-shard perspective-seq order is NOT a contract — per-key LWW (by HLC) suffices. **The LWW resolution point is the name-index bind `(ns,name)→comp_id`** (NOT compaction — adversary finding A corrected the architect: each write mints a fresh comp_id, so both land in different compaction groups; the name bind is the only LWW point, and today it's pure overwrite-by-Raft-apply-order with no perspective-seq).

**Refinement adopted (from adversary finding D): fan-includes-leader.** The durability fan always targets the current shard leader + (min_acks−1) others, so the leader always holds every acked intent locally → its committer drains its own store → no separate IntentForward RPC, and recovery-on-election is only the backstop for a leader-change-mid-fan. (A stale-leader fan is caught by the new leader's election recovery.)

### Must-fix checklist (the adversary's REJECT→ACCEPT-WITH-CHANGES list — all required before merge)
1. **Per-name perspective-seq guard = THE correctness mechanism** (replaces the watermark). Carry `perspective_seq` in the Create/Update payload (the delta HLC is `log_index`, not the seq); add a durable `(ns,name)→seq` map in BOTH storage backends, snapshot-consistent; LWW-guard the bind (bind only if incoming seq ≥ stored) at ALL sites: in-batch `bind_name`, persistent `name_insert`/`apply_hydration_batch`, `stage_create` re-bind, `stage_delete` unbind. (Finding A+B)
2. **Wire recovery to the election hook** + make the committer **leader-only** (today: spawned every node, recover() only called in tests). No-loss (R1/R5) currently rests on dead code. (Finding C)
3. **fan-includes-leader** (or a steady-state leader-pull safety net) to kill permanent-invisibility-without-an-election. (Finding D)
4. **Idempotency dedup index** keyed on the client idempotency-key (IncorporateIntent has no key field today; the seq floor does NOT catch a client retry with a fresh seq + same key) — or honestly downgrade the contract to at-least-once. (Finding E)
5. **Followers self-prune** their intent store against their own applied `max_incorporated_seq` (only the leader prunes today → unbounded follower growth at 35k–125k PUT/s). (Finding F)
6. **State-machine append-gate authoritative**: `IncorporateIntent` apply must SKIP if `seq <= max_incorporated_seq` (it appends unconditionally today; the floor is read from a stale leader cache). (Finding G)
7. **Recovery gather threshold = `cluster_size − min_acks + 1`** (not `majority`). The shipped `recover_pending` `majority` guard IS unsafe for RF=6/min_acks=2 (4+2=6 can be disjoint). Flag the wide-shard double-fault liveness regression honestly. (Confirmed-correct math)
8. **Two gate BDDs on a REAL multi-node ClusterHarness** (shared-store unit tests false-pass replication, per #127): (a) the analyst's reorder scenario (S2 forwarded-first, S1-later → newer write must win); (b) 2-node concurrent-same-name convergence. These are the merge gate.

### Code disposition
- **Delete:** `compute_stability_watermark`, `next_pending_seq` gather + `INTENT_NEXT_PENDING_TAG`, the every-node committer spawn.
- **New:** the `(ns,name)→seq` durable map + guarded bind (×4 sites) + payload seq; leader-only committer + election-recovery hook; fan-includes-leader; idempotency dedup index; follower self-prune; SM append-gate.
- **Reuse:** durable IntentStore/FjallIntentStore, `put_intent_and_fan` (retarget to include leader), `IncorporateIntent`, `recover_pending` (fix the threshold), `RaftLogIncorporationSink`, the perspective-seq.

**Verdict:** ACCEPT-WITH-CHANGES. Direction sound; the 8 must-fixes + the 2 multi-node gate BDDs are the path to a correct, robust implementation. Medium/Low findings (H clock-skew liveness, I ingress RYW divergence, J incorporator-wedge watchdog, K skew LWW, L deposed-leader spin) tracked in PART 3.

---

## PART 5 — COLLATERAL-DAMAGE SWEEP (pre-implementation sanity check)

Verdict: the LeaderSink DIRECTION breaks no other ADR/invariant (POSIX stays sync per ADR-013; S3 async per ADR-014; nothing requires perspective-seq total order; watermark deletion has zero outside consumers). But a naive implementation breaks two things — added to the must-fix list:

- **MF-9 (Blocker): the name-bind `(ns,name)→comp_id` is shared by the SYNC (POSIX/NFS/FUSE) path, which carries NO perspective-seq.** The seq-guard (MF-1) must engage ONLY when the delta carries a perspective-seq (async); sync writes bind/rename/unbind UNCONDITIONALLY as today (Raft commit order is authoritative — close-to-open). So the payload seq is `Option<PerspectiveSeq>` (Some=async-guarded, None=sync-always-bind), and every bind site (in-batch `bind_name`, `name_insert`/`apply_hydration_batch`, `stage_create` re-bind, `stage_delete` unbind) checks presence. Cross-surface same-name race → documented "sync is authoritative." This keeps POSIX byte-for-byte unchanged. (composition.rs name encode/decode + hydrator.rs:105/534/620 + persistent/storage.rs:503)
- **MF-10 (Blocker, forward-looking): split/merge committer lifecycle.** On shard split/merge/retire, the leader-only committer thread must be stopped (not orphaned) and pending intents must not be stranded (split must partition the IntentStore by hashed-key range; merge must union by perspective-seq). Split/merge isn't fully production-wired today, so this isn't exercised by the single-shard landing — but wire the committer teardown into `unregister_shard`/retire now so it can't leak. (split.rs, merge.rs, raft_shard_store.rs committers map)
- **MF-11 (Concern): confirm + document the I-CS2 staleness bound** for the S3 async-visibility window (IntentForward/fan + leader tick + Raft round + hydrator poll); confirm the `is_async_ack_eligible` POSIX gate is airtight end-to-end (it is — ops.rs). I-V3 read-your-writes holds same-node (local optimistic composition); cross-node S3 is bounded-stale per I-CS2/F-3 (by design, not a regression).

Clean to delete (no outside consumers): `compute_stability_watermark` + its tests; the steady-state `next_pending_seq` gather + `INTENT_NEXT_PENDING_TAG` (recovery uses `gather_pending` full-store, not next_pending — confirm during impl). KEEP + adapt: name-replication / hydrator tests (must still hold under the seq-guard); `intent_put_and_fan` ack-path tests. ConsumerWatermarks/I-L4 GC watermark is a DIFFERENT mechanism — untouched.

---

## PART 6 — POST-IMPLEMENTATION FINDING: the global floor is unsafe for multi-writer

**Found during implementation review of the committed LeaderSink rework (142669c).** The deliberation (MF-6 + adversary finding G) endorsed a global `max_incorporated_seq` floor as the authoritative idempotency gate. Implementation revealed this is unsafe.

### The bug
The floor is used in two places — the drain filters `seq > floor`; the follower self-prune deletes `seq ≤ floor`. That's correct only under strict perspective-seq-ordered incorporation with no gaps. LeaderSink does NOT guarantee that:

- node-A: write to **K1** (older HLC `S_a`); node-B: write to **K2** (newer HLC `S_b`). Both fan to leader.
- `S_b` arrives first → incorporated → floor = `S_b`.
- `S_a` arrives ~a tick later (jitter/slow ingress). Now `S_a < floor`:
  - leader drain filters it out → never incorporated, AND
  - follower self-prune deletes it (`S_a ≤ floor`).
- `S_a` was min_acks-durable (acked) but is now gone → **K1's write silently lost** (no-loss violation).

Masked under fast LAN (both in same 50ms tick → drain together in order). Reachable under jitter or concurrent multi-node writes to different keys. The cross-node test (single write) didn't catch it.

**Root cause:** a *global* floor cannot distinguish "already incorporated" from "older-but-never-incorporated." That distinction needs a *per-intent* record.

### Proposed fix
1. **Drain incorporates ALL local pending** (delete the `> floor` filter). Each is unique (idempotency-key dedups at `put` for keyed writes; keyless retries get fresh seqs and are at-least-once per O3), pruned after → incorporated exactly once in steady state.
2. **Follower prune becomes per-intent**: when the SM applies `IncorporateIntent(seq=X)`, prune the local intent with `perspective_seq == X` (the SM can call into the local IntentStore directly during apply — tight coupling but cleanest match). The current "prune up to floor" is replaced.
3. **Recovery dedup via recent-log-scan**: on become-leader, the new leader's recovery re-gathers pending from peers; for each re-gathered intent, dedup against a bounded *recent-incorporated-seqs* structure in the SM (replicated). A re-gathered intent that's a committed-but-unpruned-on-peer copy is caught here. The structure is bounded by the longest plausible "committed-but-unpruned" window (replication-apply lag + retry window).
4. **The SM append-gate** becomes "skip if this exact seq is in the recent-incorporated set" (per-intent, not floor). Authoritative idempotency at the apply boundary.

### Open questions for the adversary
- (Q1) Is `delete-the-floor-filter + per-intent prune` correct in **steady state**, given fan-includes-leader + 50ms ticks? Any other re-incorporation path besides recovery? (Suspect: keyless client-retry-via-re-fan — does the FjallIntentStore's idempotency-key `put`-dedup cover it for keyed writes, and is keyless-retry double-counting a real problem given each retry mints a fresh perspective-seq at ingress?)
- (Q2) What's the **correct window** for the recent-incorporated-seqs structure? Bounded by what? Storage cost at 125k PUT/s?
- (Q3) **Snapshot/restore** of the recent-seqs structure: how big can it get, and is replicating it via Raft cheap?
- (Q4) **Refcount/chunk-state double-count**: if the log-scan dedup misses a re-incorporation (outside the window), the apply increments chunk refcounts twice. Real risk or theoretical?
- (Q5) **Per-intent prune mechanism**: SM apply calls into the IntentStore vs supervisor reads recent applied seqs and prunes. Which is correct + simpler?
- (Q6) Anything else this misses?

---

## PART 7 — ADVERSARY REVIEW OF THE FIX
*(appended by the adversary)*

**Methodology.** Verified each proposed mechanism against the actual code at commit `142669c` on `design/047-decoupled-write-ack`. My prior PART-3 review endorsed the global floor and missed PART-6's bug; that miss conditions this review. Every load-bearing claim of the proposed fix has been cross-checked against `crates/kiseki-log/src/intent_committer.rs`, `intent.rs`, `raft/state_machine.rs`, `raft_shard_store.rs`, `raft_intent_sink.rs`, `intent_sync.rs`, and the gateway's perspective-seq mint in `kiseki-gateway/src/mem_gateway.rs:639-650, 2680-2696`.

Below: a finding per attack-question, severity-rated. Verdict + must-fixes at the end.

---

### Finding M (Q1.a) — Keyless client retry inflates compositions and orphans chunks under BOTH old and new design; the fix does not regress, but does not fix it either.

- **Severity:** Medium
- **Category:** Correctness > idempotency / Robustness > orphan state
- **Location:** `kiseki-gateway/src/mem_gateway.rs:639-650` (`next_perspective_seq`), `:2680-2696` (intent mint, `idempotency_key` is `Some` only when client supplied an exactly-16-byte token, else `None`), `:1024` (`create_with_name` mints a fresh `Uuid::new_v4()` per attempt), `kiseki-log/src/raft/state_machine.rs:343-362` (`apply_new_chunks` is `or_insert_with`).
- **Description:** A client PUT without a 16-byte `idempotency_key` (the common case — S3 path always nulls it per `s3.rs:137`, NFS likewise per `nfs.rs:116`, pNFS DS per `pnfs_ds_server.rs:710`) retried at the protocol layer mints (a) a fresh `comp_id = Uuid::new_v4()`, (b) a fresh `perspective_seq` (HLC tick), (c) a fresh chunk set if encryption is random-nonce (orthogonal to PR #104's deterministic nonce). Both intents are durable, both reach the leader's store, both pass the drain-all (no floor filter, and even with the floor filter both have seq > floor), both apply: two `Create` deltas at distinct tips, same name, two separate compositions. The newer-seq Create's name bind wins LWW (after Finding-B's seq-guard lands), the older-seq Create's composition is **orphan**: never name-bound, never deleted, its chunks' `cluster_chunk_state.refcount` stays at 1 (the second `apply_new_chunks` is a no-op for the same chunk_id because of `or_insert_with`; for fresh content-addressed chunk_ids the refcount is 1 on each). The proposed fix does not change this — drain-all under the OLD floor already incorporates both (each has `seq > floor` when added).
- **Evidence:** All S3/NFS/pNFS protocol surfaces hardcode `idempotency_key: None`. Only the **native** protocol carries a client key (`native/server.rs:518, 587`), and even there the gateway only honors it when the bytes are exactly width-16 (`mem_gateway.rs:2688-2691`); a 1..=64 B native key not of width 16 still degrades to `None`. The TODO at `mem_gateway.rs:2684-2687` (a stable 16-byte derivation) is unwritten.
- **Suggested resolution:** Two independent fixes, both belong in the *production* hardening (and were already pending as Finding E):
  1. **Derive a 16-byte intent-level idempotency key from the protocol-layer request token + tenant + op + name** (a domain-separated HKDF or BLAKE3, deterministic, single-attempt-stable across protocol retries). That collapses every retry to the same `IntentStore::put` row (Duplicate), and only ONE composition is ever produced. This closes the orphan-composition leak across all surfaces.
  2. If exactly-once is out of scope for this PR, **document the orphan-composition cost** and gate a periodic GC sweep that retires un-bound, no-version-history compositions older than a window. The chunk side is already safe (refcount stays 1 with `or_insert_with`).

  The proposed PART-6 fix does NOT address this; do not let the fix's correctness debate distract from this pre-existing hole. Mark it as "known, not regressed."

---

### Finding N (Q1.b) — Keyed-client-retry dedup IS crash-safe in `FjallIntentStore`. The fix preserves this. Not a hole.

- **Severity:** Informational
- **Category:** Correctness > idempotency
- **Location:** `kiseki-log/src/intent.rs:529-560` (`FjallIntentStore::put`), `:587-616` (`prune`).
- **Description:** Verified: `FjallIntentStore::put` (a) holds a mutex covering the check + commit, (b) writes the intent row + dedup pointer in a SINGLE `batch.commit()`. A crash mid-commit replays the WAL all-or-nothing — there is no window in which a dedup pointer outlives its intent row or vice versa. A keyed retry on a DIFFERENT node similarly hits the local-then-fan path: if the previous attempt's intent reached this node's store (it would, since the fan includes the leader and the leader is reachable in the common case), the second `put` returns `Duplicate` carrying the original seq, the fan dispatcher credits the ack (`intent_sync.rs:181-186` "Recorded OR Duplicate both mean the intent is now durable on this replica — credit the ack either way"), and no second store row exists. The retry is exactly-once on the dedup-keyed path.
- **Caveat:** This only fires if `WriteRequest.idempotency_key` is supplied AND happens to be exactly 16 bytes wide (gateway gate at `mem_gateway.rs:2688-2691`). For the S3/NFS/pNFS surfaces that hardcode `None`, this dedup is dormant. See Finding M.
- **Suggested resolution:** None for the dedup itself. The "derive a 16-byte key" change in Finding M lights it up for all surfaces.

---

### Finding O (Q1.c) — Fan retry to a peer that already holds the intent: ack still counts, no double-store. Verified safe.

- **Severity:** Informational
- **Category:** Correctness > idempotency
- **Location:** `kiseki-log/src/intent_sync.rs:181-191` (dispatcher: Recorded **or** Duplicate → ack credited); `intent.rs:188-195` (`InMemIntentStore::put` returns Duplicate on key collision; for keyless, the seq is globally unique and `by_seq.insert` overwrites in place — a re-fan of the same seq writes the same value).
- **Description:** A re-fan to the same peer (a) for a keyed intent: idempotency-key already present → returns `Duplicate(existing_seq)`, dispatcher counts the ack, no second row. (b) For a keyless intent: same `perspective_seq` collides on the `by_seq` BTreeMap key → second `insert` overwrites the row with byte-identical contents → counts as Recorded but adds nothing. **No double-incorporation hazard from a fan retry**, regardless of the floor design.
- **Suggested resolution:** None.

---

### Finding P (Q1.d) — Old-leader-didn't-prune-then-loses-leadership: under the OLD shipped design this DOES re-drain, but the SM append-gate catches it. Under the proposed fix, the per-intent dedup MUST be authoritative.

- **Severity:** High (a design-correctness obligation on the PART-6 fix, not yet specified)
- **Category:** Correctness > concurrency / state-machine idempotency
- **Location:** `raft_shard_store.rs:1011-1014` (leader-loss is logged + drain stops, but pruning is not synchronously flushed), `:994-1002` (self-prune runs on every node every tick, including a deposed leader, but is keyed on the applied `max_incorporated_seq` — which moves only when an `IncorporateIntent` Raft round commits and applies); `intent_committer.rs:167-204` (`Committer::run` reads the floor THEN drains).
- **Description:** Race trace under the SHIPPED (committed `142669c`) code path: node-L is leader. Tick T: drain incorporates intent I (seq S). The `client_write` succeeds (the Raft round commits). The SM apply on every replica advances `max_incorporated_seq` to S and (the *shipped* gate) returns `Appended(tip)`. Between client_write success and the apply running, **L is deposed** by a new election. Two interleavings: (i) the apply runs on L before the deposition → floor moves on L → next supervisor tick's self-prune drops S from the local store → safe. (ii) The apply lags (or never runs on L because the leadership change interrupts the apply pipeline) → L loses leadership with S still in its IntentStore and the **applied** floor still below S. Now L is a follower; the new leader L' drains its own store. If L' didn't have S (S was a partial fan that only landed on L), the new leader's `recover()` gather pulls L's pending — S is included — restored into L's local store — and then drain incorporates S. On L's side, the self-prune now runs against L's applied floor; the new leader's `IncorporateIntent(S)` replicates back, L applies it, floor advances to S on L, self-prune drops S. **The OLD floor-gate (shipped) catches the duplicate in the SM apply** — `state_machine.rs:438-446` "if `perspective_seq <= max_incorporated_seq`, return `Appended(self.tip)` no-op." That gate is the safety.

  Under the **PART-6 proposed fix** the floor is gone; the SM append-gate becomes "skip if `perspective_seq` is in the recent-incorporated-seqs set." So the dedup correctness depends entirely on: (a) the structure being populated on the apply path **before** any re-incorporation can occur, and (b) the structure being replicated identically and snapshot-included so a leader that was just promoted reads the same set. Both are achievable but neither is in the proposal as code. The doc says "replicated in the SM" but does not specify *when* a seq enters the set (must be: at the same apply point that today moves `max_incorporated_seq`, i.e., the first IncorporateIntent for that seq).
- **Evidence:** No teardown-on-deposition flushes the apply queue or guarantees the floor has caught up; self-prune is async on the supervisor tick (`raft_shard_store.rs:994-1002`), so a leadership loss between client_write and apply is fully reachable. The shipped SM gate at `state_machine.rs:438-446` is what saves us today.
- **Suggested resolution:** Specify the per-intent set membership rule precisely: **a seq is added to the recent-incorporated-seqs structure inside the same `apply` block that appends the delta**, in the same SM mutation. That makes it impossible for a second apply of the same seq (re-gather or re-fan) to slip through, because Raft serializes applies per replica. Add an SM unit test that drives `IncorporateIntent(S)` twice and asserts the second is a no-op.

---

### Finding Q (Q2) — The recent-incorporated-seqs structure is critically under-bounded. At production throughput it is the new SPOF: under-size and you lose writes; over-size and snapshots / replication / restart explode.

- **Severity:** Critical
- **Category:** Robustness > resource exhaustion / Correctness > idempotency
- **Location:** PART-6 §"Proposed fix" item 3, item 4. No corresponding code yet.
- **Description:** "bounded by the longest plausible committed-but-unpruned window" is not a specification — it is a wish. Concretely:
  - **(a) recovery-dedup window.** A re-gathered intent stays in a peer's IntentStore until that peer's self-prune fires. Self-prune is driven by the **peer's own applied** `max_incorporated_seq`, which advances when the IncorporateIntent's Raft apply lands on that peer. So `unprune_window ≥ max(per-peer Raft-apply-lag) + supervisor_interval (50 ms shipped) + the fjall prune scan cost`. Under healthy replication this is sub-second. Under partition recovery, or a follower that fell behind by minutes and is catching up via log replay or snapshot install, the unprune window is **the catch-up duration** — minutes to hours on a snapshot install on a wide cold replica. The dedup structure must cover that window or recovery will double-incorporate. This is not a tunable, it is a *correctness* bound coupled to operational tail latencies.
  - **(b) steady-state keyless-retry window.** Q1.a's retries are NOT caught by this structure at all (each retry mints a fresh seq) — the structure dedups by `perspective_seq`, not by content. So this window is irrelevant for keyless retries. **Strike the "covers retries" justification.** Keyless-retry dedup is Finding M's problem, separate.
  - **(c) storage cost.** At 125k PUT/s sustained (the GCP perf target), with a 60-second window (a realistic ceiling for routine apply lag), the set holds 7.5M seqs. `PerspectiveSeq` is `HybridLogicalClock { physical_ms:u64, logical:u32, node_id:NodeId(u64) }` = 20 bytes raw; a `BTreeSet<PerspectiveSeq>` node has overhead. Conservative: 64 B/entry → **480 MiB per shard, per replica**, replicated, in the SM snapshot. For a deployment with ~32 shards/node that's 15 GiB of SM state just for the dedup structure. **This is not "remotely sustainable" at scale.** Even at 10k PUT/s and a 10-second window it is 100k entries × 64 B = 6.4 MiB/shard — fine, but the bound is set by the worst-case operational window, not the steady state.
  - **(d) bounding scheme.** A Bloom filter is the right answer for steady-state membership (constant size for tunable FPR), but a Bloom CANNOT be safely combined with "if-not-in-set then append": a false positive **silently drops a legitimate intent** (R1 no-loss violation). Time-windowed deque + hash index has the right semantics but the size problem above. **An LRU is wrong** — eviction discards in arrival order, not seq order, and a slow apply can evict a still-needed seq. Honest answer: **the structure must be a time-bounded set whose window is `max(replication-apply-lag) + safety`, and that bound is operationally determined.** Snapshot/restart cost has to be eaten — there is no safe-and-cheap shortcut.
- **Evidence:** PART-6's bounding hand-waves "longest plausible." The proposal lists Bloom as a tunable-FPR alternative; a Bloom on this hot path violates R1 every time it false-positives.
- **Suggested resolution:** **REJECT the unbounded-structure framing.** Specify concretely:
  1. The window MUST be `max_applied_lag + supervisor_interval + safety_margin`, where `max_applied_lag` is an OPERATIONAL constant (e.g. P99.9 of the apply pipeline lag, with a hard cap that triggers a circuit-breaker if exceeded — refuse to drain and surface an alarm).
  2. The structure is a **(perspective_seq → log_index) map**, ordered by perspective_seq (a BTreeMap), pruned by log-index age — i.e., "drop any entry whose log_index is more than W indices behind the current applied index." This couples the dedup window to Raft-log progress, not wall time (which means a stalled shard does not silently shrink its dedup window).
  3. Document a hard ceiling on entries — say 10M — and refuse to admit beyond it (treat that as a circuit-breaker, halt and alarm rather than evict-and-lose).
  4. Provide explicit storage math in the ADR per (cluster_PUTs/s, target_window, shard_count) so deployment can budget snapshot size BEFORE turning the gate on.

  **Without this, the fix is correct in principle but unshippable in practice.**

---

### Finding R (Q3) — Snapshot/restore of the recent-seqs structure: must be in the SM snapshot. Will measurably inflate install-snapshot cost on wide cold replicas.

- **Severity:** High
- **Category:** Robustness > snapshot/restart cost
- **Location:** `raft/state_machine.rs:55-79` (`ShardSnapshot` struct; `max_incorporated_seq` is already snapshot-included via `#[serde(default)]`), `:555-605` (`build_snapshot` serializes the inner state), `:642-672` (`install_snapshot` restores it).
- **Description:** The shipped snapshot is `delta_count + tip + maintenance + deltas + watermarks + shard_id + tenant_id + max_incorporated_seq` — the deltas dominate the size (delta payload is the actual data). Adding a recent-incorporated-seqs map adds: per entry, ~20 B raw seq + a few B for the log-index → ~32 B/entry serialized (postcard). At Finding Q's 7.5M-entry ceiling, that's ~240 MB **per shard snapshot**, replicated to every newly-joining or catching-up follower. Today's `install_snapshot` (line 648-665) `serde-decodes` the whole blob in one pass; a 240-MB blob is multi-second on a single-thread decoder before the new follower can answer reads. This regresses I-L8 (snapshot install bounded) and the shard-restart latency budget the codebase already assumes (verified: no explicit snapshot size budget exists in the tree; grep for `snapshot.*size` yields only test bounds, so the contract is implicit). The fix's correctness obligation forces the structure into the snapshot — there is no skipping it — so the cost is unavoidable. **It must be budgeted.**
- **Evidence:** `install_snapshot` does `postcard::from_bytes` on the whole `data: Vec<u8>` synchronously (line 653-658 range). No streaming decode, no chunked install. At Finding-Q sizes this is a multi-second pause on join.
- **Suggested resolution:** (a) Window the structure aggressively (Finding Q); a 1-second window at 125k/s is 125k entries × 32 B = 4 MB — acceptable. (b) Document a snapshot-size budget per shard in the ADR (e.g. "≤ 64 MB without deltas") and assert the dedup structure size at build_snapshot time; refuse to grow it beyond the budget (alarm and circuit-break). (c) Consider a separate, **non-replicated** local cache for the recent-seqs structure plus a smaller **replicated** authoritative tail (the last K seqs by log-index), so the snapshot stays small and a freshly-installed follower rebuilds its cache by replaying the tail of the log. The replicated tail is the dedup source-of-truth; the local cache is a perf prefilter. This mirrors the current floor design without the multi-writer hole.

---

### Finding S (Q4) — Refcount / chunk-state double-count if dedup misses: real but bounded. The `or_insert_with` idempotency in `apply_new_chunks` already covers the per-chunk side; the orphan composition + delta-row inflation is the residual cost.

- **Severity:** Medium
- **Category:** Correctness > orphan state / Robustness > resource exhaustion
- **Location:** `raft/state_machine.rs:343-362` (`apply_new_chunks`: `or_insert_with` is idempotent on `(tenant, chunk_id)`), `:268-338` (`append_delta_inner`: ALWAYS bumps tip and pushes a delta), `:447-460` (`IncorporateIntent` apply: calls `apply_new_chunks` then `append_delta_inner` then advances floor).
- **Description:** Verified: a duplicate `IncorporateIntent(seq=S)` apply does NOT double-increment `cluster_chunk_state.refcount` — the entry is created with `refcount: 1` on first apply and `or_insert_with` is a no-op on re-apply (line 352-360). So the chunk side is safe. **But** `append_delta_inner` (line 336 `self.deltas.push(delta)`, line 268 `self.tip += 1`) ALWAYS appends a new delta and advances the tip. A duplicate apply produces TWO delta rows for the same hashed_key, at different tips, with different inline-data keys (`inline_key = hashed_key XOR tip` at line 294-301 means the second apply WRITES the inline payload to a different inline-store key). Consequences:
  1. **Compaction merge:** `compaction_worker.rs` groups by `hashed_key`, sorts by Raft sequence, keeps the newest min_versions. Both deltas have the same `hashed_key` (the create-delta encodes the same comp_id → same hashed_key) → compaction collapses them. The orphan delta is reclaimed at compaction time, not at apply time.
  2. **Pre-compaction read amplification:** between the duplicate apply and the next compaction, scans for that hashed_key return 2 rows; the hydrator sees BOTH `Create` deltas. `stage_create` is idempotent on the comp-already-staged path (`hydrator.rs:534-538` returns Applied + re-binds the name), so no double-composition. But it **re-binds the name** — under the un-shipped Finding-B seq-guard that is a no-op (incoming seq == stored seq, tie); without it the name index may flap.
  3. **Inline-data leak:** the second apply writes inline data to a different inline-store key (`tip` differs). That row is never referenced by any composition (the comp_id only references the first delta's inline_key) and is never GC'd by the deltas index. **Real inline-store leak**, bounded by the duplicate rate × payload size. At 125k PUT/s with a 0.001% dedup miss (a slow apply lag escaping a 1-second window once per million) = ~0.1 leaks/s × inline payload size (≤ 64 KiB inline threshold). Bounded but real, and the leak has no GC.
- **Evidence:** `append_delta_inner` line 336; the inline-key formula at line 294-301; no inline-store GC pass tied to delta compaction in `compaction_worker.rs`.
- **Suggested resolution:** (a) The fix's authoritative SM dedup MUST land — the duplicate is supposed to be impossible. If Finding Q's window is honest, this is a vanishingly rare event (and circuit-broken before it can amplify). (b) **Tighten the apply path:** check membership BEFORE calling `append_delta_inner` AND `apply_new_chunks`, so a duplicate is a true no-op (no new chunk-state, no delta row, no inline write). The shipped floor-gate already does this (`state_machine.rs:441-446` returns early without touching state); the proposed per-intent gate must too. (c) Add a follow-up GC for orphan inline rows keyed on tip-XOR-hashed_key when the corresponding tip is compacted away. Track as a follow-up; do not block.

---

### Finding T (Q5) — Per-intent prune mechanism: "SM apply calls into the IntentStore directly" is the wrong coupling. The supervisor-side prune is already there and correct.

- **Severity:** High
- **Category:** Correctness > layering / Robustness > failure-modes
- **Location:** PART-6 §"Proposed fix" item 2; existing supervisor self-prune at `raft_shard_store.rs:994-1002`; `IntentStore` trait at `intent.rs:117-138`.
- **Description:** Three sub-questions:
  1. **(a) Will the SM hold an `Arc<dyn IntentStore>`?** Structurally possible — there's precedent: `inline_store: Option<Arc<dyn InlineStore>>` at `state_machine.rs:191` is exactly this pattern. So this is feasible.
  2. **(b) An `IntentStore` `prune` failure during apply — does it block the Raft apply (bad) or get swallowed (worse)?** Per `openraft`'s contract, the SM `apply` returns a response synchronously; an error here either propagates as a `RaftError` (which openraft treats as fatal — the node halts the apply pipeline) OR is `Result::unwrap_or_else`'d into a swallow. EITHER outcome is bad:
     - **Halt-the-apply-pipeline:** a transient fjall I/O hiccup on the intent store halts Raft apply on that node — the shard becomes blind to all subsequent commits, including chunk refcount mutations, watermark advances, etc. This is the "bad" path the question warns about.
     - **Swallow:** the intent stays in the store; the SM advances; on the next supervisor self-prune the floor will (under the OLD design) catch it. **But under the proposed fix the floor is gone** — the supervisor self-prune is now per-intent against the recent-seqs set. So a swallowed prune-failure leaks the row until... when? The supervisor self-prune needs to be able to retry from a record OUTSIDE the SM (in fjall) — which is exactly what the supervisor-side prune does today.
  3. **(c) Alternative: supervisor reads recently-applied IncorporateIntent seqs from the Raft log + prunes off-band.** This is **what's shipped today** (`raft_shard_store.rs:994-1002`): every supervisor tick reads `leadership_store.max_incorporated_seq().await` and calls `prune_store.prune(...)`. Under the per-intent design, the supervisor needs to read the recent-seqs set (or its tail) and prune those specific intents — same I/O shape, just per-intent instead of `<= floor`.

  **(c) is strictly safer than (a)+(b)**: it decouples consensus apply from the intent-store I/O fault domain. An intent-store stutter does not halt Raft; a Raft hiccup does not corrupt the intent store. The retry semantics are also natural (the supervisor re-attempts each tick). The "tight coupling — cleanest match" phrasing in PART-6 is a layering smell that trades real fault-isolation for an aesthetic of fewer hops.
- **Evidence:** `inline_store` precedent at `state_machine.rs:191` (so the coupling pattern exists), supervisor self-prune at `raft_shard_store.rs:994-1002` (so the off-band prune pattern exists and works).
- **Suggested resolution:** **Keep the prune off-band in the supervisor**, NOT in the SM apply. Specifically: change the supervisor self-prune from "prune ≤ applied floor" to "prune any local intent whose seq is in the applied recent-incorporated-seqs set." The SM owns the dedup set (replicated, snapshot-included); the supervisor reads it each tick and prunes accordingly. This preserves the fault-isolation that the global-floor design accidentally got right.

---

### Finding U (Q6) — Drain-all under load: real risk. Needs a batch cap, especially under restart / recovery / migration storms.

- **Severity:** High
- **Category:** Robustness > unbounded operations / liveness
- **Location:** `intent_committer.rs:167-204` (`Committer::run` drains ALL pending in one pass, sorts, then `sink.incorporate(&selected)`); `raft_intent_sink.rs:138-150` (`incorporate` iterates and does ONE Raft `client_write` per intent, each blocking the dedicated committer thread).
- **Description:** Today: drain-all sorts the entire pending set and calls `sink.incorporate` ONCE with the whole slice. The sink iterates the slice and `block_on(append_intent)` per element — so it is N sequential Raft rounds. At 125k PUT/s steady state with 50 ms ticks, a healthy run is 6.25k intents/drain; one Raft round per intent, sequentially blocked on the committer thread, is hopelessly slow (a Raft round is ~ms, so 6.25k rounds = 6+ seconds per drain — drain falls further behind every tick). **The shipped numbers must be lying or the drain coalesces somewhere I missed; either way the drain-all-N-rounds path is a structural latency bomb under load.**

  Under recovery, a freshly-promoted leader restores its `peer_pending` union into its local store BEFORE draining (`raft_shard_store.rs:1018-1044`). If the partition was a few minutes long at 125k PUT/s on a busy shard, that union can be tens of millions of intents. Drain-all attempts ONE incorporate call with the whole set, blocking the committer thread the entire time. The supervisor never returns to the leadership check, never returns to the shutdown-signal `tokio::select!`. **This is a hang risk.** Add to that: the proposed fix's per-intent SM dedup must process every one, replicated, snapshot-impacted.
- **Evidence:** `Committer::run` line 173-185 `store.pending()?.into_iter().filter(...).collect()` — unbounded. Line 195 `sink.incorporate(&selected)?` — unbounded. The sink iterates and blocks per intent.
- **Suggested resolution:**
  1. **Batch the drain**: cap each `drain_local` pass at K intents (e.g. K = 256-1024, tunable). Run multiple ticks back-to-back if the queue is deep (event-wake the supervisor when the pending depth crosses a watermark). Documents a hard per-pass cap.
  2. **Batch the Raft append**: prefer batching multiple `IncorporateIntent` commands into a single `client_write` if the openraft commands support it (verify — `LogCommand` is an enum; a `LogCommand::IncorporateIntents(Vec<...>)` variant would consolidate the Raft round cost). This is a separate ADR follow-up but is the actual perf lever.
  3. **Periodic shutdown-check**: even mid-drain, the supervisor must yield to `shutdown.changed()` between batches so shard-retire / split / merge / `Drop` does not block.

  **Without (1) the recovery hang is real**; without (2) the latency target is unreachable at 125k PUT/s.

---

### Finding V (Q7-a) — Leaving `max_incorporated_seq` around as "the highest seq ever incorporated": benign as a metric, dangerous if anything else reads it as a gate.

- **Severity:** Medium
- **Category:** Correctness > obsolete state / Robustness > maintenance hazard
- **Location:** `raft/state_machine.rs:78, 220, 459` (the field; advanced on every `IncorporateIntent`); `intent_committer.rs:171-185` (the committer reads it as `max_inc` and drops anything `<= m`); `raft_shard_store.rs:998` (the supervisor self-prune reads the applied `max_incorporated_seq` to prune the store).
- **Description:** Under the PART-6 fix, the floor is no longer the drain filter (item 1 deletes that), no longer the gate (item 4 replaces with per-intent set), no longer the prune cutoff (item 2 replaces with per-intent prune). **Three consumers** of the floor exist in the shipped code. The proposal addresses all three but leaves the field. As a pure metric ("highest seq ever applied") it is fine. As a **read** by any future code path treating it as authoritative idempotency, it is a multi-writer hole reborn. The PART-6 race that motivated the fix (S_b applied first, S_a < floor → drain-filter drops it AND self-prune deletes it) was caused by exactly this kind of misread.
- **Evidence:** Three current readers of `max_incorporated_seq`; the fix specifies replacements for all three but the field remains and is freely advanced.
- **Suggested resolution:** Either (a) **delete the field** along with the three readers — there is no other consumer (verified: grep shows no other use sites), or (b) **rename it** to `highest_observed_seq` and add a `#[deprecated]` doc comment plus a `clippy::disallowed_methods`-style lint to prevent re-introducing a floor-gate misuse. Strongly prefer (a). Carrying an unused gate field is a latent debt; the next adversary review (or refactor) will misread it.

---

### Finding W (Q7-b) — Recovery dedup vs un-restored recent-seqs structure on snapshot install: real race.

- **Severity:** High
- **Category:** Correctness > startup ordering
- **Location:** `raft/state_machine.rs:642-672` (`install_snapshot` restores the SM state including the structure once Finding R lands); `raft_shard_store.rs:1018-1052` (supervisor: on becoming leader, runs `gather_pending` + `recover()` + drain in sequence each tick); the recovery threshold guard `cluster_size − min_acks + 1` at `intent_committer.rs:256-265`.
- **Description:** Trace: node-L was offline, comes back, joins as a follower, openraft sends it an `install_snapshot`. The snapshot includes the recent-incorporated-seqs structure (per Finding R). install_snapshot decodes synchronously into the SM. Meanwhile a leader election may complete on L itself (e.g. preferred-leader handoff) — `is_leader()` flips true. The supervisor on L sees the edge, runs `gather_pending` from peers, calls `recover()`, `restore_into` the local store, then drains. **Each step of drain calls `IncorporateIntent` whose SM apply consults the recent-seqs structure to dedup.** If the snapshot install has not yet completed when the drain begins, the structure is empty (or partial) and **every re-gathered seq is treated as new** → mass double-incorporation. The shipped global-floor design has the same race in principle but is safer in practice because the floor is a single `Option<HybridLogicalClock>` — a partial snapshot install either has it or not, and openraft's snapshot install is atomic at the apply boundary. A bulk structure is NOT atomic at the consumer level: even if install_snapshot writes it atomically, ANY interleaving where the supervisor reads `is_leader()=true` BEFORE the SM-side install completes is a window.

  Additionally: the recovery threshold `cluster_size − min_acks + 1` is checked against the **gathered** peer count, NOT against the structure's readiness. A node that just got a fresh snapshot installs the seqs but has not yet seen any post-snapshot IncorporateIntent applies. If those applies pump up the recent-seqs structure, the supervisor must wait for the structure to be "current up to the last applied log index" before draining. There is no such gate today.
- **Evidence:** `install_snapshot` at line 648-665, supervisor edge check at `raft_shard_store.rs:1006-1014`. Nothing serializes the two.
- **Suggested resolution:** Add a **structure-ready gate** in the supervisor: after `gather_pending` + `recover()`, BEFORE draining, assert that the SM's `last_applied_log_index >= snapshot_install_log_index` AND the recent-seqs structure's "as-of" index is current. Concretely: track an `applied_log_index` on the SM (already present implicitly via `tip`) and an explicit `dedup_structure_last_applied` field; the supervisor reads both and refuses to drain until they agree with `raft.metrics().last_applied`. Same logic as the post-snapshot "wait for log catch-up" pattern used elsewhere in `kiseki-log`. Without this, snapshot install + leader-promotion-in-the-same-window is a double-incorporation generator.

---

### Finding X (Q7-c) — Per-name seq-guard (MF-1/9) is orthogonal: it does NOT compensate for chunk-refcount / inline-store / delta-row inflation. Different consumers, different scopes.

- **Severity:** Informational (clarification of scope)
- **Category:** Correctness > scope
- **Location:** PART-2 §5 + Adversary Finding B (the per-name seq-guard); PART-5 MF-9 (Option-typed seq for sync vs async); the cluster_chunk_state path in `state_machine.rs:343-362`.
- **Description:** The per-name seq-guard, when shipped, makes name-bind LWW-by-perspective-seq at hydrator apply time. It compensates for **name-binding** races (Finding A/B's "S2-forwarded-first / S1-later" reorder, and Finding I's RYW divergence). It does **NOT** compensate for:
  - Chunk-state refcount inflation (Finding S, side 1 — covered by `or_insert_with`, not by the seq-guard).
  - Delta-row inflation in the log (Finding S, side 2 — covered by compaction, not by the seq-guard).
  - Inline-store row leak on duplicate apply (Finding S, side 3 — NO mechanism covers this without an inline GC pass).
  - Orphan composition objects from keyless retries (Finding M — covered by deriving an idempotency key, not by the seq-guard).

  So: the per-name seq-guard is **necessary but not sufficient** for the LeaderSink direction. The PART-6 fix's per-intent dedup is the chunk-state / delta-row / inline-leak defense; the per-name seq-guard is the name-bind defense. Both required.
- **Evidence:** N/A — scope clarification.
- **Suggested resolution:** Document this orthogonality in the ADR's "Correctness mechanisms" section so future readers don't conflate them and trim one as redundant.

---

### Finding Y (Q7-d) — Drain-all + leader-loss-mid-drain: the recovery pulls back what the old leader was about to drop. Edge but real.

- **Severity:** Medium
- **Category:** Correctness > leader-handoff races
- **Location:** `raft_shard_store.rs:1011-1014` (leader-loss only logs + skips draining next tick — there is no in-flight cancellation of an active drain pass); `intent_committer.rs:167-204` (drain holds `&dyn IntentStore` and `&mut dyn IncorporationSink` for the entire pass, including the prune at line 199-201).
- **Description:** Trace: leader L is mid-drain. It has incorporated intents I_1..I_k (each via a Raft `client_write` that returned Ok) and is about to call `store.prune(I_k.perspective_seq)`. Election happens; L is deposed before prune runs. L_new is elected. L_new's supervisor runs `gather_pending` — L is still up (just deposed) and answers with its full local store, which still contains I_1..I_k (not pruned). Merged into L_new's local store. L_new's drain incorporates I_1..I_k. **The proposed per-intent dedup catches them** at the SM apply, because their seqs are in the recent-seqs structure (assuming Q3/W are addressed). So this is safe IF Q2/Q3 are honest. Under the SHIPPED floor design this is also safe (floor gate). So the proposed fix preserves the safety but inherits all of Finding Q's structural fragility.

  A second flavor: L is mid-drain, mid-`client_write` for I_k. The `client_write` returns `ForwardToLeader` because L just lost leadership — the openraft check fires post-submission, pre-apply. L's drain returns Err on that pass (`raft_intent_sink.rs:145` `.map_err(|e| IntentError::Incorporate(e.to_string()))`). The supervisor logs and continues (`raft_shard_store.rs:1049-1051`). The intent is NOT pruned (the prune in `Committer::run:199-201` runs AFTER successful incorporate). The intent survives in L's local store. L_new's recovery picks it up. **Safe** under the per-intent dedup (it WASN'T incorporated; correctly re-tried; per-intent set doesn't list it; incorporated by L_new).
- **Evidence:** `Committer::run:195-201` (prune is conditional on incorporate succeeding); `raft_shard_store.rs:1049-1051` (drain error is logged and the tick continues).
- **Suggested resolution:** No code change required IF Findings Q/R/W are honored. Document the case in the ADR's "Failure modes deliberately handled" list so future reviewers don't trip over it.

---

### Finding Z (Q7-other) — Supervisor self-prune races against drain in the SAME process. Mostly benign but worth tightening.

- **Severity:** Low
- **Category:** Concurrency
- **Location:** `raft_shard_store.rs:994-1002` (self-prune is the FIRST step every tick) and `:1048-1051` (drain is the LAST step every tick, on the same supervisor thread, but `committer.drain_local()` is sync and `prune_store.prune()` is sync — they don't interleave within one tick). **However**, on a leader, the same `intent_store` is shared between `committer.local_store` (used by drain) and `prune_store` (used by self-prune). Both grab the fjall `mutations` lock; no deadlock (separate sync calls), but a prune mid-iter of a drain's `store.pending()` would be a data race. Verified safe because drain calls `store.pending()` → returns owned `Vec<WriteIntent>` THEN consumes it, releasing any borrow before prune runs. Per-call serialization via the fjall mutation lock.
- **Description:** Confirmed safe. The supervisor's single-threaded structure (one tick = self-prune → leadership check → recover → drain) is the saving grace; if the supervisor were ever split across multiple threads / tasks, a concurrent self-prune + drain would need explicit ordering.
- **Suggested resolution:** Add a doc comment on the supervisor pinning the single-threaded contract so a future refactor doesn't accidentally split it.

---

### Finding AA (other) — Drop-the-floor-filter breaks an existing recovery guarantee: a re-gathered intent that WAS already in the log re-applies and re-binds the name.

- **Severity:** Critical
- **Category:** Correctness > idempotency / recovery
- **Location:** PART-6 §"Proposed fix" item 1 ("drop the `> floor` filter"); current `Committer::run` floor filter at `intent_committer.rs:177-185`; current SM apply-gate at `state_machine.rs:438-446`.
- **Description:** Under the SHIPPED design, the SM apply-gate is the authoritative idempotency floor: re-applied IncorporateIntent with `seq <= max_incorporated_seq` is a no-op (line 441-446). The committer-side filter at `intent_committer.rs:177-185` is a perf prefilter only. Under the proposed fix, the committer-side filter is **deleted** (no `> floor`), and the SM gate becomes per-intent (only seqs IN the recent-seqs set are skipped). **For a recovered intent whose seq was applied a LONG time ago — outside the recent-seqs structure's window — the structure does not contain it. The SM apply-gate has nothing to compare against. The recovered intent re-applies → re-pushes the delta, re-binds the name, advances the tip.** This is double-incorporation when the structure window is too small for the recovery scenario.

  Concretely: a long-partitioned follower's intent store may contain intents that ARE already in the Raft log but were applied weeks ago and pruned out of the recent-seqs structure. When that follower comes back and becomes leader, its recovery gather pulls in this stale intent set, drain-all attempts to incorporate them, the SM has no record, **mass re-application of historical writes**. The per-name seq-guard prevents the LWW from regressing (the older intent has older seq, loses to the current binding) — but the chunk-state side and inline-store side (Finding S) still suffer. And every re-applied intent's delta row is a duplicate that compaction has to clean up.

  **This is the new R1/R5 hole opened by deleting the floor filter.** The shipped floor filter caught these "ancient" recoveries via `seq <= floor` (always true for an ancient intent). The proposed per-intent set is bounded; ancient intents fall out of the window; the gate misses.
- **Evidence:** PART-6 §"Proposed fix" item 1: "Drain incorporates ALL local pending (delete the `> floor` filter)." `Committer::run:177-185` shows the filter being deleted. SM gate at `state_machine.rs:441-446` shows the floor-based skip being replaced. The recent-seqs structure's window is finite (Finding Q).
- **Suggested resolution:** **Do not delete the floor filter at the committer side. Re-frame:** keep the floor as a `(last_applied_log_index, max_seq_at_or_before_that_index)` watermark — but make it a **per-replica observed-applied** floor, not a global "drop everything ≤" floor. The PART-6 race was specifically about the global-applied floor being used to filter LATE-ARRIVING-LOW-SEQ intents that were never incorporated. The fix: the floor is "the highest seq that has been APPLIED on this replica," NOT "the highest seq that has been incorporated cluster-wide." A late-arriving lower seq that is NOT in the recent-seqs structure AND is BELOW this replica's applied floor is unambiguously ancient → drop. A late-arriving lower seq that IS in the recent-seqs structure → drop (duplicate). A late-arriving lower seq above the floor's *log-index* window → admit.

  This is a refinement, not a contradiction: the floor stays as an "ancient-cutoff" prefilter; the recent-seqs structure becomes the precise duplicate detector for the recent window. Together they cover (ancient + recent). The PART-6 race is resolved because the new floor is gated on log-index, not on cluster-applied seq, so a "newer-by-seq-but-older-by-log-index" intent is admitted (no false drop).

  **This is the simpler-and-correct mechanism PART-6 missed**: keep the floor, change its semantics to log-index-gated rather than seq-gated; add the per-intent set ONLY for the recent window. Total complexity is the same, both holes closed.

---

### Finding AB (other) — The supervisor's `is_leader()` is a snapshot, not a watched edge. Long Raft elections can produce phantom recoveries.

- **Severity:** Low
- **Category:** Robustness > observability
- **Location:** `raft_shard_store.rs:1005` (`leadership_store.is_leader()` polled per tick).
- **Description:** `is_leader()` reads a metric snapshot. During an election the value can flap (candidate → leader → stepped-down within hundreds of ms if a higher-term vote arrives). Each false→true edge resets `recovered_this_term = false` and re-runs `gather_pending()` (a multi-peer RPC). At 50ms supervisor ticks, a flapping leadership produces a recovery storm — costly but not incorrect (recovery is idempotent on the local store).
- **Evidence:** `is_leader()` is a `bool` snapshot, not a watch. The supervisor edge detector is `was_leader` boolean — no debounce.
- **Suggested resolution:** Watch openraft's `metrics()` for `current_leader` changes with a debounce (e.g. require N consecutive ticks of stable leadership before treating it as a new term). Or watch `ServerState::Leader` via the openraft metrics stream rather than polling. Minor tightening.

---

### VERDICT: **REJECT-AS-WRITTEN → ACCEPT-WITH-CHANGES** with a reframe (Finding AA).

The PART-6 diagnosis of the bug is correct: a global, perspective-seq-keyed floor used as a drain-filter is unsafe for multi-writer late arrivals. My PART-3 endorsement of that floor was wrong; I missed that the floor's semantics conflated "incorporated" with "highest-observed-and-applied," and the late-arrival case sits in the gap.

But the proposed fix has critical structural problems:

1. **Finding Q (Critical)** — the recent-incorporated-seqs structure is unbounded as proposed. At production throughput (125k PUT/s) and realistic operational windows (apply lag tails of seconds-to-minutes on partition recovery), the structure size is hundreds of MiB per shard per replica, replicated, snapshot-included. This is not shippable without a hard window discipline AND a circuit-breaker for over-cap.
2. **Finding AA (Critical)** — deleting the committer-side floor filter outright opens a new R1/R5 hole: a long-partitioned follower's recovery dumps ancient intents that have fallen out of the recent-seqs window. The SM gate misses them, mass re-application ensues. The proposal needs a log-index-keyed ancient-cutoff floor (which is NOT the same as the broken seq-keyed cluster-applied floor) PLUS the per-intent recent-seqs structure for the recent window. Two complementary mechanisms, both finite.
3. **Finding P (High)** — the per-intent dedup MUST be specified as "set membership updated in the SAME `apply` block as the delta append, atomically." Without that pin, the deposed-leader-mid-apply race is open.
4. **Finding R (High)** — snapshot inflation is unavoidable; budget it explicitly and assert in build_snapshot.
5. **Finding T (High)** — SM-side prune via `Arc<dyn IntentStore>` is the wrong coupling. Keep the prune off-band in the supervisor (it's already there); the supervisor reads the recent-seqs set and prunes per-intent. Same I/O shape, full fault-isolation.
6. **Finding U (High)** — drain-all has no batch cap; recovery on a busy shard hangs the committer thread. Add per-pass cap K, batch the Raft append, yield to shutdown between batches.
7. **Finding W (High)** — snapshot install + leader promotion race opens a mass double-incorporation window unless the supervisor explicitly waits for the dedup structure to be "current as of the last applied log index" before draining.

**Reframe (simpler mechanism PART-6 missed):** Keep the floor as a **log-index-keyed ancient-cutoff filter** (the highest seq applied on THIS replica's log up to the last-applied-log-index), used by the committer as a "drop anything older than the apply-prefix of this replica's log" prefilter. Add the recent-incorporated-seqs structure ONLY for the recent window (the gap between the ancient cutoff and the current applied tip). The ancient cutoff is a single watermark — small, snapshot-trivial, replicated — and catches the partitioned-replica recovery case (Finding AA). The recent-seqs structure is bounded (Finding Q) and catches the multi-writer late-arrival case (PART-6's bug). The two together cover the failure spectrum.

**Must-fix list (blocking):**
1. Adopt the Finding-AA reframe: log-index-keyed ancient cutoff + bounded recent-seqs structure. Specify both mechanisms in the ADR.
2. Specify the bounding discipline for the recent-seqs structure (Finding Q): log-index-windowed, hard ceiling, circuit-break on overflow, snapshot-size assertion.
3. Pin the "set membership updated in the same apply block" rule (Finding P).
4. Keep the prune off-band in the supervisor; the SM owns the set, the supervisor prunes the store (Finding T).
5. Batch-cap the drain-all and add a between-batch shutdown yield (Finding U).
6. Specify the structure-ready gate in the supervisor: refuse to drain until the recent-seqs structure is current-as-of-last-applied (Finding W).
7. Delete or rename `max_incorporated_seq` so its semantics don't drift back (Finding V).
8. Add an SM unit test that drives `IncorporateIntent(S)` twice in the SAME apply pipeline and asserts the second is a no-op. Add an integration test on a real multi-node `ClusterHarness` that reproduces the PART-6 race (concurrent K1 on node-A, K2 on node-B, jitter the arrival order, assert both visible after drain).

**Should-fix (non-blocking but document):**
9. The keyless-client-retry orphan composition (Finding M) — derive a 16-byte idempotency key for all surfaces, OR document the orphan cost + add an orphan-composition GC.
10. Inline-store row leak on duplicate apply (Finding S item 3) — track as a follow-up with bounded GC.
11. Supervisor leadership debounce (Finding AB) — minor noise reduction.

**Endorsement caveat — I was wrong once.** My PART-3 endorsement of the global floor missed the multi-writer hole. I am now endorsing a more layered design (ancient cutoff + recent-seqs structure). I have argued through each Q1-Q7 attack and verified the code. The remaining residual risk is concentrated in **Finding Q's window calibration** — that's an operational dial whose wrong setting silently re-opens a no-loss hole. The ADR MUST land with explicit calibration math AND a runtime alarm on apply-lag exceeding the configured window, OR it should not land at all.

---

## PART 8 — DECISION ON THE FIX (reframe, accepted)

Per PART 7 adversary review (verdict REJECT-as-written → ACCEPT with the reframe):

**Accepted design — two complementary mechanisms:**
1. **Ancient cutoff:** a log-index-keyed per-replica-applied watermark (NOT perspective-seq-keyed). Intents older than the cutoff are refused-with-alarm, never silently dropped. Catches long-partition recovery (Finding AA).
2. **Bounded recent-incorporated-seqs set:** in the SM (replicated), covering the window between cutoff and applied tip. Hybrid bound (e.g. last 100k seqs OR ≤60s, whichever first). Hard-capped, circuit-breaker on overflow, alarm + refuse-with-error if apply-lag exceeds window. Catches multi-writer late-arrival + steady-state re-incorporation. NOT a Bloom (false-positives would silently drop legitimate intents).

**Plus the must-fixes from PART 7:**
- **P:** dedup-set update is atomic with the delta append in the same SM apply block.
- **T:** per-intent prune stays **off-band in the supervisor** (the existing pattern at `raft_shard_store.rs:994-1002`), NOT in SM apply. SM owns the set; supervisor reads it (or reads applied IncorporateIntent seqs from the log tail) and prunes per-intent. Preserves Raft/store fault isolation.
- **U:** **`IncorporateIntents(Vec<...>)`** batched log command with a per-pass cap (e.g. 1000) and yields to shutdown.
- **W:** supervisor blocks draining post-leader-promotion until recent-seqs is current as of last applied log index.

**Gate:** the concurrent-DIFFERENT-key multi-node BDD (the case that exposed the original floor bug) must pass. Plus the same-name concurrent BDD (LWW under reorder — still needs the per-name seq-guard MF-1/9).

Verified safe (no fix needed): keyed-retry crash-safe (Finding N — `FjallIntentStore` atomic batch); fan retry Duplicate-counts-as-ack (Finding O). Per-name seq-guard (MF-1/9) is orthogonal — necessary but not sufficient (Finding X).
