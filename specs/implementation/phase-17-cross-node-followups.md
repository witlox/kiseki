# Phase 17 — Cross-Node Follow-ups

**Status**: Planned
**Date opened**: 2026-04-30
**Predecessor**: Phase 16f (composition hydrator) closed cross-node S3
correctness; the items below are the architectural debt that Phase 16f
deliberately did not take on.

## Context

Phase 16a wired chunk fan-out via `ClusteredChunkStore`. Phase 16f added
the composition hydrator so followers can resolve `composition_id` from
the Raft-replicated delta log. Together they make
`tests/e2e/test_cross_node_replication.py` pass 4-of-4 against a 3-node
docker-compose cluster: PUT on any node, GET from any other node, reads
survive a single-node failure, quorum-lost writes return 503.

Three categories of follow-up surfaced during 16f and were explicitly
bounded out:

| # | Item | Role | Size | Blocks |
|---|------|------|------|--------|
| 1 | Update / Delete delta hydration | implementer | ~80 LOC + 2 tests | overwrite + delete cross-node |
| 2 | Persistent metadata stores (`CompositionStore` + `ViewStore`) | architect → implementer | ADR-040 + ~600 LOC | scale beyond ~1M compositions / node |
| 3 | Snapshot integration for compositions + views | architect → implementer | ADR-040 amendment + ~200 LOC | late-joining nodes after log truncation |
| 4 | Per-shard leader endpoint | implementer | ~50 LOC + 2 tests | clean retry semantics for clients |

Items 2 and 3 share an ADR (ADR-040) because they describe two faces of
the same change: making metadata state persistent unifies the snapshot
path and the steady-state lookup path. Items 1 and 4 are pure
implementer work — no new architectural decisions.

---

## Item 1 — Update / Delete delta hydration (implementer-only)

### Why

`CompositionHydrator::poll` only handles `OperationType::Create` today.
A subsequent PUT on the same key emits an `Update` delta; followers
keep the original `chunks` / `size`. `Delete` leaves a tombstone on
the leader but the composition stays alive on followers. A real S3
client doing overwrite or delete on a multi-node cluster will diverge
silently from the leader.

### Scope

- `crates/kiseki-composition/src/composition.rs`: extend the
  payload encoding for Update + Delete (Update needs new chunks +
  size, Delete needs only the comp_id). One way: a `u8` discriminator
  byte at offset 0, then per-operation fields. Keep the Create
  encoding bit-compatible by treating the existing 40-byte form as
  "discriminator = Create implicitly" in the legacy decoder.
  Cleaner: bump the encoding to a small struct serialized via
  `bincode`/`postcard` so the layout is self-describing. Architect
  optional here — the encoding is internal to one crate.
- `CompositionStore`: add `update_at(comp_id, chunks, size)` and
  `delete_at(comp_id)` mirroring `create_at`. Both must be
  idempotent (apply-twice = no-op).
- `crates/kiseki-composition/src/hydrator.rs`: extend
  `CompositionHydrator::poll` to dispatch on `delta.header.operation`.
- `crates/kiseki-gateway/src/mem_gateway.rs`: update path emits the
  Update payload; delete path emits the Delete payload.

### Acceptance

- New unit tests in `hydrator.rs`:
  `hydrator_applies_update_delta_replaces_chunks_and_size`,
  `hydrator_applies_delete_delta_removes_composition`.
- New e2e tests in `test_cross_node_replication.py`:
  `test_overwrite_visible_on_followers_after_settle`,
  `test_delete_visible_on_followers_after_settle`. Both follow the
  same shape as `test_cross_node_read_after_leader_put` — write,
  sleep 1 s, verify on every node.
- Existing 4 cross-node tests stay green.

### Dependencies

None. Standalone work.

### Risk

Low. Same shape as the Create path that's already proven.

---

## Item 2 — Persistent `CompositionStore` and `ViewStore` (architect first)

### Why

Both stores hold every composition / view ever created in
`HashMap<Id, T>`. At billions of objects this exhausts node RAM. The
composition record is ~280 bytes today (per ADR-030 §1's metadata
table); a million compositions is sub-GB and fine, ten million is
~3 GB and starting to hurt, a billion is a non-starter. The view
store has the same shape and the same wall.

### Architect scope (DONE — see ADR-040 rev 2)

ADR-040 (`specs/architecture/adr/040-persistent-metadata-stores.md`)
captures the decisions across two revisions:

- **rev 1** (commit `a08e479`): initial structural choices —
  redb-backed sibling stores, postcard encoding with a leading
  schema-version byte, hot-tail LRU, sync-only inner locks (kept
  off the await path), atomic `last_applied + state` redb
  transactions, and a two-regime snapshot story (D6.1 today,
  D6.2 deferred for when openraft log compaction lands).
- **rev 2** (this commit): addresses adversary findings F-1..F-7
  inline. §D5.1 specifies the transient/permanent skip algorithm
  (new invariant I-CP6); §D5 drops the conflated
  `last_applied_log_index`; §D6.3 specifies the
  sequence-comparison gap-detection rule that doesn't need a
  new `LogOps` API; §D7 makes the gateway read-retry budget
  configurable + observable; §D8.1 places `PersistentStoreError`
  in the error taxonomy; §D10 specifies 13 observability
  metrics; §D11 explicitly scopes persistence to `compositions`
  only (namespaces + multiparts stay in-memory).

Six invariants `I-CP1`..`I-CP6` added to `specs/invariants.md`.

**Adversary status**: rev 1 findings F-1..F-7 (Critical + High)
addressed in rev 2 inline. F-8..F-17 (Medium + Low) deferred to
implementation review. **Rev 2 adversary sign-off** conditionally
accepted: two new Medium findings (N-1, N-4) carried as required
implementation-review tickets I-1 and I-2. Architect rev 3 not
required; implementer can start.

### Implementation-review tickets carried with the impl PRs

- **I-1 (closes N-1)**: persist the transient-skip retry counter.
  Add two `meta` keys to the redb (`stuck_at_seq`,
  `stuck_retries`); on boot, read both — if
  `stuck_at_seq == last_applied_seq + 1` resume the counter,
  otherwise reset. Required so I-CP6's exhausted-retries alarm
  fires reliably in crash-loop scenarios where the in-memory
  counter resets on every boot. Revise I-CP6 to add: "the retry
  counter is durable in the same redb transaction as
  `last_applied_seq`."
- **I-2 (closes N-4)**: gateway returns HTTP 503 (with
  `Retry-After`) for composition lookups that miss the local
  persistent store **when the hydrator is in halt mode**. Phase
  17 item 4's `/cluster/shards/{id}/leader` endpoint should also
  surface a halt-mode flag. Required so load balancers and
  multi-gateway clients route around a halted node instead of
  caching the spurious 404.

The auditor verifies both tickets are addressed at Gate 2.

### Original architect scope (now satisfied):

1. **Storage layout.** Reuse the existing redb-backed pattern (one
   `.redb` per logical store, separate from the chunk redb). Keys:
   `CompositionId` (16-byte UUID) and `ViewId`. Values: the existing
   `Composition` / `ViewDescriptor` structs serialized via
   `postcard` (already a workspace dep) or `bincode`.
2. **Hot tail caching.** The hydrator and gateway are read-heavy on
   recently-written compositions. Keep an LRU in front of the redb
   reader so warm reads don't pay disk latency. Bound: 100 k entries
   default, configurable.
3. **Lock semantics.** redb transactions are sync. Today the
   `CompositionStore` lives behind a `tokio::sync::Mutex` because
   the gateway holds it across awaits. With redb, the store
   internally uses `std::sync::Mutex` for the LRU but releases it
   before the redb transaction starts. The gateway-side lock pattern
   doesn't change at the call sites — it stays a `tokio::sync::Mutex`
   wrapping the `CompositionStore` as a whole, because the gateway
   still derives state across awaits (e.g. checking view staleness).
4. **Crash semantics.** redb is fsync-on-commit. Hydrator commits
   per-batch (one transaction per `poll()` call, batching N deltas).
   On crash, the next hydrator startup reads `last_applied` from a
   single-key meta table inside the redb and resumes from there.
5. **Migration.** First-boot detection: empty redb → install the
   bootstrap "default" namespace (the deterministic UUID), no
   migration. Existing in-memory stores have no on-disk presence
   today, so there's no upgrade path to write — only a future
   downgrade story (out of scope; one-way migration is fine for now).
6. **Concurrency with the existing Raft state machine.** The Raft
   state machine already persists chunk state to its own redb. The
   composition redb is a sibling; no shared transactions. The
   hydrator reads from the Raft delta log (which is already
   persistent) and writes to the composition redb — no new
   coordination problem.

Adversary review of the ADR before implementation. Concerns to address:
- What if the hydrator crashes between `last_applied` update and the
  composition write? The compositions are written first inside the
  same redb transaction as `last_applied`; either both land or both
  don't (transactional). Document this explicitly.
- What if two hydrators run concurrently (e.g. during a redeploy)?
  redb is single-writer. The second hydrator's `WriteTransaction::new`
  blocks. Document that there's at most one hydrator per node.

### Implementer scope (after ADR)

- New crate module `kiseki-composition/src/persistent.rs`:
  redb-backed `PersistentCompositionStore` implementing
  `CompositionOps + create_at`. Mirror for `kiseki-view`.
- Constructor wires open-or-init at boot, hot-tail LRU initialization.
- Runtime swap: `comp_store = if let Some(dir) = cfg.data_dir { ... persistent ... } else { ... in-memory ... }`. Tests stay on
  in-memory.
- Bench: insert 10 M compositions, read p50/p99 from the hot tail
  after warm-up; document numbers.

### Acceptance

- ADR-040 accepted with adversary sign-off.
- All existing tests green (in-memory remains the default for tests).
- New integration test: spin up persistent compositions, write 10 k
  records, restart server, verify all 10 k still resolve.
- Local `du -sh data/compositions.redb` post-test stays within 5 MB
  for 10 k records (sanity check on encoding size).

### Dependencies

Architect ADR is the gate. Implementer can start as soon as that lands.

### Risk

Medium. Persistent state changes the boot path, the snapshot path
(Item 3), and the failure-recovery path. The ADR's job is to surface
the failure modes before code lands.

---

## Item 3 — Snapshot integration for compositions + views (couples with Item 2)

### Why

Raft compacts the log via snapshot. A node joining late (or recovering
from a long pause) bootstraps from the snapshot rather than replaying
truncated deltas. Today the snapshot encoding doesn't include
compositions or views, so a late-joining node misses every composition
that existed before the snapshot. This is invisible in CI (no log
truncation in steady-state runs) but real in any long-running cluster.

### Architect scope

Amendment to ADR-040 (or a sibling ADR if the snapshot story justifies
its own document — judgment call by the architect during ADR-040
drafting). Decisions:

1. **What goes in the snapshot.** The persistent redb files for
   compositions + views are themselves the snapshot — no extra
   serialization step. Snapshots become "bundle the redb files +
   the existing chunk-state snapshot + a manifest" instead of "encode
   in-memory state to a Vec<u8>".
2. **Transfer protocol.** redb files can be sizeable (multi-GB at
   scale). Streamed file transfer via the existing Raft snapshot
   transport, chunked. Architect decides whether to reuse the
   existing snapshot RPC or add a sidechannel.
3. **Atomicity.** A late-joining node receives the snapshot, writes
   the redb files to a staging dir, atomic-renames into place,
   opens them. If the snapshot RPC fails midway, partial files in
   the staging dir get cleaned on next startup.

### Implementer scope

- Wire `PersistentCompositionStore` + `PersistentViewStore` into the
  Raft snapshot encode/decode path.
- Test: spin up a 3-node cluster, kill node-2, advance the leader
  far enough to trigger log compaction, bring node-2 back, verify
  it hydrates from the snapshot rather than replaying truncated
  deltas, verify all compositions resolve.

### Acceptance

- ADR-040 amendment accepted.
- New e2e test passes (the late-joiner-after-truncation scenario
  above). Note this test takes minutes — log truncation needs the
  truncate threshold to be hit. Make the threshold configurable so
  the test can force it.
- Manual: 24-hour soak with a script that PUTs at 100 ops/sec; kill
  node-2 mid-soak, bring it back at hour 23, confirm convergence.

### Dependencies

Item 2 is the hard prerequisite. Snapshot integration without persistent
state is meaningless.

### Risk

Medium-high. Snapshot semantics are subtle and the failure modes
(partial transfer, mid-snapshot leader change) are exactly the cases
where bugs hide. Adversary review pre-implementation is essential.

---

## Item 4 — Per-shard leader endpoint (implementer-only)

### Why

`/cluster/info` reports a cluster-level `leader_id`. The actual
write-path semantics are per-shard: a write to shard X can fail with
`LeaderUnavailable: ShardId(X)` even when `cluster/info` shows a
healthy leader (Raft elections are per-shard, and the cluster-level
endpoint reports the membership leader, not necessarily the data-shard
leader). I papered over this in `test_cross_node_replication.py` with
a 30-second `_put_object` retry; production clients deserve a clean
"is shard X writable right now?" answer they can poll.

### Scope

- `crates/kiseki-server/src/admin.rs` (or wherever `/cluster/info`
  lives): add `GET /cluster/shards/{shard_id}/leader` returning
  `{leader_id, term, last_committed_seq}` or 404 if the shard
  doesn't exist on this node.
- `kiseki-log` exposes the per-shard leader info from openraft —
  it already has it internally; just needs a getter on
  `RaftShardStore`.
- Update `_put_object` retry in the test to poll the per-shard
  endpoint instead of relying on the elapsed-time heuristic.

### Acceptance

- New unit test: spawn a 3-node `kiseki-log::raft::test_cluster`,
  query the per-shard leader endpoint on each node, assert all
  three return the same `leader_id`.
- New integration test in `tests/e2e/`: kill the leader, poll
  `/cluster/shards/.../leader` on a follower, observe the
  `leader_id` change once election completes.
- `_put_object` retry in `test_cross_node_replication.py` uses
  the new endpoint; total test runtime drops from ~85 s to ~30 s.

### Dependencies

None.

### Risk

Low. Pure plumbing — exposes existing state via a new HTTP route.

---

## Sequencing

```
Item 1 ──┐
Item 4 ──┴── (independent, both implementer-only, can be done in parallel)

Item 2 (ADR-040 + persistent stores)
   │
   └── Item 3 (snapshot integration, depends on Item 2)
```

Recommended order:
1. **Items 1 and 4 first** — small, independent, immediate user value
   (overwrite/delete cross-node correctness; clean shard-leader
   semantics). Two PRs, can land in either order.
2. **Item 2 (ADR-040 + implementation)** — bigger, needs architect
   pass. Don't start until Items 1 and 4 are merged so the test bench
   is stable.
3. **Item 3** — fold into the same ADR / phase; goes hand-in-hand
   with Item 2.

---

## Out of scope (explicitly)

The following came up during 16f and are intentionally **not** taken
on in Phase 17:

- **Read-after-write across gateways with strict watermark.** The
  current bounded retry in `mem_gateway::read` (~1 s) is sufficient
  for the test patterns we care about. A full client-side seq
  threading mechanism (PUT response carries `last_seq`, GET sends
  `If-Seq-At-Least`) is a real protocol-design effort and waits
  until a customer asks for it.
- **Composition rename / multipart cross-node.** Same shape as
  Update/Delete (Item 1) but more code surface; defer until Item 1
  has shaken out the encoding.
- **Multi-shard cluster cross-node.** Today's hydrator polls one
  shard (`bootstrap_shard`). When ADR-033 multi-shard topology lands
  in production, the hydrator iterates over all shards on this node.
  That's coupled to ADR-033's rollout, not to this phase.
- **Cross-tenant namespace creation replication.** Followers only
  see the bootstrap "default" namespace (a hardcoded convention).
  Tenant-created namespaces aren't replicated to followers yet —
  the same architectural shape as compositions, scaled up to
  tenant-managed entities. Phase 18 territory.
