# ADR-040: Persistent Metadata Stores (CompositionStore, ViewStore)

**Status**: Proposed (pending adversary review)
**Date**: 2026-04-30
**Deciders**: Architect role; implementer to execute after adversary sign-off
**Context**: Phase 17 items 2 + 3 follow-ups from
`specs/implementation/phase-17-cross-node-followups.md`
**Related ADRs**: ADR-026 (Raft topology), ADR-029 (raw block allocator),
ADR-030 (small-file placement), ADR-032 (async gateway ops),
ADR-036 (LogOps shard management)

## Problem

Phase 16f closed cross-node correctness for the gateway path: chunks
fan out via fabric, and the composition hydrator (Phase 17 item 1
extension) keeps each follower's `CompositionStore` in sync with the
Raft-replicated delta log. But both `CompositionStore` and `ViewStore`
are still in-memory `HashMap<Id, T>` structures populated entirely by
hydration:

- **Composition record**: ~280 B per object (per ADR-030 §1's metadata
  table). 1M compositions/node ≈ 280 MB; 10M ≈ 2.8 GB; 1B is a
  non-starter on any reasonable host.
- **View record**: similar shape, similar wall.
- **Hydration cost on restart**: a node that restarts replays every
  delta in its log, calling `create_at` per record. With 10M
  compositions this is many seconds of cold-start hydration during
  which the gateway returns 404 for everything (the read-path retry
  budget is 1 s; longer hydration → spurious failures).
- **Late-joining nodes**: a node that joins after openraft compacts
  its log (future, not yet enabled in the codebase) will receive a
  Raft snapshot rather than the full delta history. The current
  hydrator has no way to seed compositions/views from a snapshot — it
  only knows how to fold Create/Update/Delete deltas. So a node that
  joins post-compaction misses every composition that existed before
  the snapshot.

ADR-040 makes both stores persistent (redb-backed), sequence-tracked,
crash-safe, and snapshot-aware so all three problems get handled by
the same change.

## Scope

- `CompositionStore` and `ViewStore` get redb-backed siblings:
  `PersistentCompositionStore` and `PersistentViewStore`. The
  in-memory implementations stay (test fixtures use them; single-node
  deployments without `KISEKI_DATA_DIR` use them too).
- Hydrator gains durable `last_applied_seq` + a snapshot-reseed entry
  point so a Raft `install_snapshot` event resets the persistent
  state correctly.
- Out of scope: the existing `cluster_chunk_state` redb owned by the
  per-shard Raft state machine (already persistent, already
  snapshot-aware, already correct for its layer). Compositions and
  views are siblings, not children, of that store.

## Decision

### D1. Storage layout

One redb file per logical store, sibling to existing per-node redbs:

```
KISEKI_DATA_DIR/
├── chunks/                            existing — block-device + manifest
│   ├── data.dev
│   └── meta.json
├── small/                             existing — ADR-030 inline content
│   └── objects.redb
├── raft/                              existing — openraft log + state
│   └── log.redb
└── metadata/                          NEW (ADR-040)
    ├── compositions.redb
    └── views.redb
```

Each redb has two tables:

```rust
// compositions.redb
const COMPOSITIONS: TableDefinition<'_, &[u8;16], &[u8]>
    = TableDefinition::new("compositions");      // value = postcard(Composition)
const META: TableDefinition<'_, &str, &[u8]>
    = TableDefinition::new("meta");              // see D4

// views.redb
const VIEWS: TableDefinition<'_, &[u8;16], &[u8]>
    = TableDefinition::new("views");             // value = postcard(ViewDescriptor + ViewState)
const META: TableDefinition<'_, &str, &[u8]>
    = TableDefinition::new("meta");
```

**Why redb (not sled, not lmdb-rs):**
- Workspace already uses redb in `kiseki-chunk::SmallObjectStore` and
  in the openraft state-machine log store. Adding redb is zero
  dependency growth.
- Single-writer / multi-reader transactional model matches the
  hydrator-only-writes shape (only one hydrator per node; gateway
  reads come from the LRU cache layer below).
- Crash-consistent on commit (fsync on `WriteTransaction::commit`).

**Why postcard (not bincode, not JSON):**
- `postcard` is in the workspace already; bincode is not.
- Compact, deterministic, no schema drift via field reordering when
  derived `serde::Serialize`.
- Schema versioning via a leading `u8` discriminator on each record.
  See D8.

### D2. Encoding

Each `Composition` and `ViewState` record is encoded as:

```
[1 byte: schema version, currently 1]
[postcard-serialized struct]
```

Decoders match on the leading byte; an unknown version returns a
typed `SchemaTooNew` error so a node running an older binary against
a newer redb refuses to corrupt state silently. See D8 for the
upgrade path.

### D3. Hot-tail caching

A bounded LRU sits in front of each redb reader. Hits skip the redb
transaction; misses fall through and populate the cache:

```rust
pub struct PersistentCompositionStore {
    db: Arc<Mutex<redb::Database>>,    // sync; redb txns are sync
    cache: Arc<Mutex<LruCache<CompositionId, Composition>>>,
    last_applied: Arc<Mutex<SequenceNumber>>,
}
```

**Cache size**: `KISEKI_COMPOSITION_CACHE_ENTRIES` env, default
100 000. At ~280 B/record that's ~28 MB max — cheap. Operators
sizing for billions of total objects don't cache them all; the
working-set assumption is that recently-written compositions are the
read-hot ones.

**Eviction**: classic LRU. The `lru` crate is in the workspace dev
chain via `kiseki-client::cache`; promote to a regular dep.

**Cache-vs-redb consistency**: the hydrator holds the redb write
transaction and updates the cache *after* the txn commits, in the
same critical section. A reader that races the writer either sees
the pre-commit cache value (stale, but bounded by the next
hydrator poll interval) or the post-commit value (current). Never
torn.

### D4. Lock semantics

Three locks, three responsibilities:

| Lock | Type | Held for | Held across `.await`? |
|---|---|---|---|
| Outer `tokio::sync::Mutex<dyn CompositionStore>` (gateway-side) | tokio | full read path including chunk fetch | yes — pre-existing |
| Inner `std::sync::Mutex<redb::Database>` | std | one redb transaction | no |
| Inner `std::sync::Mutex<LruCache>` | std | cache lookup or insert | no |

**Critical rule**: the outer tokio Mutex is held across awaits (the
gateway reads chunks while holding it, pre-existing pattern from
ADR-032). The inner redb + cache locks must NEVER be held across an
await: redb txns are sync and short, the LRU update is sync and
short. The persistent store implements its trait methods as `async
fn` for API symmetry with the in-memory store, but each method's
body is non-async — it acquires the std::sync::Mutex, does its
work, releases, returns.

This avoids the documented `tokio::sync::Mutex::blocking_lock` panic
risk and is the pattern `kiseki-chunk::SmallObjectStore` already
uses.

### D5. Crash semantics — atomic `last_applied + state`

The hydrator's invariant (Phase 16f, Phase 17 item 1):
**`last_applied_seq` advances only when the corresponding state
change has been durably committed.** In-memory this is trivial; on
disk it requires both updates to land in the same redb transaction.

Per hydrator poll:

```
begin_write()
  for each delta in batch:
    apply (insert / update / delete in COMPOSITIONS table)
  meta.put("last_applied_seq", new_seq)
  meta.put("last_applied_log_index", entry.log_index)
commit()       <-- single fsync
update LRU cache
```

If the process crashes between `apply` and `commit`, redb's
journaling rolls everything back; on restart, `last_applied_seq`
reads the value from before the failed batch. If the process
crashes between `commit` and the LRU update, the next read pays a
redb miss and re-hydrates the cache — correct, just slightly slow.

Bound on lost work: at most one batch (default 1000 deltas) per
crash. The hydrator picks up where it left off on restart.

### D6. Snapshot integration

Two regimes — current and future:

**D6.1 Current (no openraft log compaction).** The state machine's
`install_snapshot` populates `inner.deltas` with every delta in the
snapshot. The hydrator on the receiving node, even with
`last_applied_seq = 0` (fresh node), polls `read_deltas` and gets
the deltas back. No new mechanism needed — the persistent store
gets populated organically as the hydrator processes deltas.

**The receiver's `last_applied_seq` is not pre-seeded by the
snapshot install.** It still starts at 0 (or whatever was in the
node's previous redb). The hydrator iterates the deltas the
snapshot exposed and converges. **This is the only behavior that
works without a coordinated bundle transfer**, and it's correct
*as long as* the snapshot exposes the full delta history (which it
does today).

**D6.2 Future (when log compaction lands).** A snapshot at log
index N replaces deltas before N with a state summary; deltas before
N are no longer visible to `read_deltas`. The hydrator's `read_deltas(from = 1)` returns nothing in `[1, N)`, only deltas
`[N+1, tip]`. Compositions created by deltas `[1, N)` are lost on
the receiving node.

Resolution (deferred, captured here so the implementer doesn't have
to redesign): the per-shard openraft snapshot grows two new fields,
`compositions_blob` and `views_blob`, each a redb-encoded byte slice
(or path-pointer to a sidechannel transfer). On `install_snapshot`,
the receiving node:

1. Stages the bundled redbs to `metadata/compositions.redb.staging`
   and `metadata/views.redb.staging`.
2. Atomic-renames the staging files into place once the snapshot
   install fully succeeds (mirror the openraft state-machine
   commit point — both succeed or both fail).
3. The `meta.last_applied_seq` in the bundled redb tells the
   hydrator where to resume.

A separate ADR ratifies the bundle format + transfer protocol when
log compaction is enabled. That ADR is **not** ADR-040; it's a
sibling that depends on this one.

**D6.3 Self-defense in D6.1 mode.** Persistent
`last_applied_seq` is checked against the log's earliest visible
delta on every poll. If `last_applied_seq < earliest_visible_seq`
(the compaction window), the hydrator emits a `tracing::error!` and
stops polling. Operator action required: drop the node's metadata
redbs, restart, let it rehydrate from the snapshot. Document this
as the "recovery procedure" until D6.2 lands.

### D7. Concurrency

- **One hydrator per node.** Enforced by the runtime (single spawn
  in `runtime.rs`). redb's single-writer transaction model would
  serialize concurrent writers anyway, but we don't rely on that —
  the runtime is the gate.
- **Gateway reads** acquire the outer tokio Mutex (existing
  pattern), then call `PersistentCompositionStore::get(comp_id)`
  which acquires the LRU mutex. Multiple concurrent reads serialize
  on the LRU mutex *only* during the brief lookup; cache hits don't
  touch redb.
- **Gateway writes (Create/Update/Delete via emit-delta path)** do
  *not* touch the persistent store directly. The leader's local
  state converges via its own hydrator, which sees the deltas the
  gateway just appended to the Raft log. This means a write-then-
  read on the leader pays the hydrator's poll latency (≤ 100 ms
  + 25 ms gateway retry = ~125 ms p99). Acceptable for an
  eventually-consistent design; the existing 1 s gateway retry
  absorbs it.

  Alternative considered: have the leader's gateway also write
  directly to its local persistent store on emit-success. Rejected
  because (a) it's redundant work — the hydrator does it 100 ms
  later anyway, (b) it bypasses the single-writer assumption and
  needs explicit write-side conflict handling, (c) it would need
  rollback on emit failure (mirroring ADR-032's rollback dance for
  in-memory state), adding complexity. The latency cost is small
  enough not to justify the duplication.

### D8. Schema versioning

Each persisted record is `[1 byte version][postcard payload]`. The
current version is `1`. When a future schema change happens:

- Backwards-compatible additions (new optional fields): bump payload
  format inside version 1; old code reads optional fields as
  `None` and forward-compatible.
- Breaking changes: version 2. Decoder for version 1 stays,
  decoder for version 2 is added. A meta-key
  `meta.put("schema_version", &[2])` records the on-disk version.
  On open: if `schema_version > binary_version`, refuse to start
  with a clear "binary too old, downgrade not supported" error. If
  `schema_version < binary_version`, run the upgrade path (read v1
  records, write v2 records, bump `schema_version`). One-shot, no
  rollback.

ADR-004 (schema versioning) covers the broader pattern; this
section says the redb stores opt into it.

### D9. Migration from in-memory

First-boot detection: opening a path that doesn't exist creates an
empty redb with `meta.schema_version = 1` and `meta.last_applied_seq
= 0`. The hydrator runs from delta sequence 1 and rebuilds the store.

For an existing cluster upgrading from Phase 16f to Phase 17 item 2:
the in-memory store is replaced by the persistent one; on first
boot, the persistent redb is empty; the hydrator processes the full
delta history. For a 10 M-composition cluster this could take tens
of seconds — acceptable as a one-time per-node cost. The gateway
read path's 1-second retry will surface as 404s during this window;
operators should drain reads from a node before upgrading it. This
is acceptable because the upgrade is a planned operation, not a
crash recovery.

## Invariants

These get added to `specs/invariants.md` (status `Proposed` until
the implementation lands, then `Confirmed`):

- **I-CP1**: A persistent `CompositionStore` advances
  `meta.last_applied_seq` only as part of the same redb transaction
  that applies the corresponding deltas. Crash between batches
  loses at most one batch's worth of work; on restart the hydrator
  resumes from the durably-committed `last_applied_seq + 1`.

- **I-CP2**: At most one composition hydrator runs per node at any
  time. Enforced by the runtime spawn; any second hydrator would be
  serialized by redb's single-writer transaction model and is
  considered an operator error.

- **I-CP3**: A persistent store record is always
  `[1 byte: schema_version][postcard payload]`. Decoders that see a
  version they don't recognize return `SchemaTooNew` rather than
  crashing or interpreting the payload as a different schema.

- **I-CP4**: The gateway's read path looks up compositions through
  the persistent store's LRU cache; cache hits do not acquire the
  redb transaction. Cache invalidation happens inside the same
  critical section as the redb commit, so a cache hit always
  reflects state at-or-after the last commit.

- **I-CP5**: When the openraft log compaction window advances past
  `meta.last_applied_seq`, the hydrator stops polling and emits an
  error log. The operator's recovery action is to drop the
  node's metadata redbs and restart; the persistent store
  re-hydrates from the snapshot. Until ADR-XXX lands this is the
  only correct behavior.

## Alternatives considered

### A1. Compositions live inside the per-shard Raft state machine

The shape that the existing `cluster_chunk_state` table uses: every
composition write goes through Raft consensus, fully replicated and
snapshotted automatically. Compositions become a peer of
chunk-state in the openraft state machine.

**Rejected** because:
- Doubles the Raft proposal cost per S3 PUT (one for chunk-state,
  one for composition).
- Breaks the existing data flow where the gateway writes compositions
  *before* emitting a delta to the log (ADR-032's lock-then-emit
  pattern). Inverting that ordering is invasive across multiple
  modules.
- The existing in-memory `CompositionStore` is independent of Raft
  consensus, which is correct: compositions are *derived* from
  deltas, not first-class Raft state. Persistence shouldn't change
  the model.

### A2. SQLite instead of redb

SQLite has the same single-writer / multi-reader model and richer
query support. Rejected because:
- Adds a new dependency (`rusqlite`) where redb is already in the
  workspace.
- The query needs are trivial — primary-key lookup + a single
  meta-key — and don't justify a query language.
- redb's API is closer to the workspace style.

### A3. Defer item 3 (snapshot integration) entirely

Implement persistent stores (item 2) without the snapshot story.
**Rejected** because the snapshot story is what makes the design
defensible for a long-running cluster. Even if compaction isn't
enabled today, designing without it means a future ADR has to
re-litigate the storage layout and the lock semantics. Capturing
D6.2 as a deferred follow-up keeps the path forward visible.

### A4. Bundle the redb file in the snapshot today (eager)

Don't wait for compaction; always bundle. **Rejected** for now
because it adds protocol surface (snapshot transport + atomic
rename) without solving a problem that exists today. D6.1 works
correctly under the current "snapshots include all deltas"
behavior. When compaction lands, D6.2 takes over. This is a "ship
the small change first, layer in complexity when needed" call.

## Consequences

### Positive

- 1 GB compositions/node ≈ 3.5 M records → fits comfortably; 10 GB
  is feasible for very-large clusters. Persistent state takes the
  in-memory wall off the table.
- Restart hydration cost paid once at first-upgrade; subsequent
  restarts pay only the meta-key read + cache warm-up.
- Late-joining nodes work today (D6.1) and will keep working when
  compaction lands (D6.2).
- The 1-second gateway read retry from Phase 16f stays load-bearing
  but doesn't grow — persistent storage doesn't change the
  hydrator's poll cadence.

### Negative

- Single-writer constraint: the hydrator is the bottleneck for
  composition application. At 1000 deltas/batch / 100 ms poll =
  10k deltas/sec. For most clusters this is several PUT/sec, well
  above the per-node S3 ceiling. If a future workload exceeds it,
  the hydrator can be sharded by composition_id-prefix.
- Read-after-write on the leader pays ~125 ms p99 from the polling
  delay. Documented; absorbed by the gateway retry.
- A schema breaking change forces a node restart with the upgrade
  path. Documented in D8.

### Neutral

- The in-memory `CompositionStore` stays as the test-fixture default
  and the single-node-no-`KISEKI_DATA_DIR` fallback.

## Open questions / future work

1. **Compaction-aware snapshot bundle (D6.2)**. ADR-XXX, deferred
   until openraft log compaction is enabled. Implementer should
   wire D6.3's self-defense check now so the failure mode is
   explicit when compaction lands.

2. **Cross-tenant namespace replication**. Same architectural shape
   as compositions (per-tenant `Namespace` records that today are
   bootstrap-only). Phase 18 territory; the persistent
   `CompositionStore`'s `namespaces` table should be designed with
   this in mind (column reserved, not yet populated).

3. **ViewStore is symmetric to CompositionStore but its update
   surface is different** (view watermarks advance per delta, not
   per record). ADR-040 covers the storage layout; the view
   stream-processor's interaction with the persistent layer is
   spelled out in `kiseki-view`'s implementation, not here.

4. **Per-shard hydrator** (when ADR-033 multi-shard topology lands
   in production): one hydrator per shard, each with its own
   `last_applied_seq`. The redb encoding pre-supports this — the
   meta keys can be namespaced as `meta.<shard_id>.last_applied_seq`.

## Adversary review

This ADR requires an adversary pass before implementation. Specific
concerns to address:

1. **Cache-coherence under concurrent reads + writes**. D3 claims
   "the hydrator updates the cache after the txn commits, in the
   same critical section." Verify the implementation actually holds
   *both* the std::sync::Mutex<Database> and the
   std::sync::Mutex<LruCache> simultaneously during the commit-and-
   update window. If they're acquired sequentially, a reader can
   slip between commit and cache-update and observe a stale cached
   value after the txn committed a new one.

2. **Hydrator restart between commit and cache update**. Same
   window as concern 1 but with a different observer: a *new*
   hydrator started after the crash. It reads `last_applied_seq`
   from the (now-committed) redb and resumes — which is correct
   because the cache is fresh on a fresh process. No real risk.

3. **Two hydrators racing at startup**. D7 says "the runtime is
   the gate," but a misconfigured operator running two server
   processes against the same `KISEKI_DATA_DIR` would have two
   hydrators. redb's `WriteTransaction::begin_write` is exclusive,
   so the second blocks indefinitely; document this as the
   operator-error symptom.

4. **`install_snapshot` mid-poll**. The hydrator is mid-batch when
   openraft installs a snapshot that replaces `inner.deltas`. The
   hydrator's view of the log shifts under it. `read_deltas` is
   transactional inside the state machine, but between calls the
   set of deltas can change. Verify the hydrator handles either:
   (a) deltas it expected to see disappearing, or
   (b) the visible delta range jumping forward.
   D6.3 adds the self-defense check for case (b); case (a) doesn't
   happen under D6.1 (snapshots include the full history).

5. **Postcard non-determinism**. Postcard's encoding is
   deterministic for `Serialize` types whose field order is
   stable. `Composition` derives `Serialize` and its fields are
   declared in a fixed order. Verify no `HashMap`/`HashSet` fields
   exist (those don't postcard-determinismize). If they do, switch
   to `BTreeMap`/`BTreeSet` or use a deterministic-postcard
   wrapper.

6. **The "hydrator is the only writer" rule vs. snapshot install**.
   D6.2 has the snapshot install staging-rename into place, which
   *replaces* the redb file under the running hydrator. The
   hydrator's open `Database` handle becomes stale. Resolution:
   the snapshot install must (a) signal the hydrator to pause via
   a `tokio::sync::watch` channel, (b) wait for the hydrator's
   current poll to finish, (c) close the hydrator's database
   handle, (d) atomic-rename, (e) reopen, (f) signal resume. This
   is non-trivial; capture as a precondition for the D6.2 ADR
   rather than inflate ADR-040.

The implementer takes the adversary's findings, addresses them in
the implementation, and the auditor verifies the addressed concerns
match the spec at gate 2.
