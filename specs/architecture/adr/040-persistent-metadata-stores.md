# ADR-040: Persistent Metadata Stores (CompositionStore, ViewStore)

**Status**: Proposed — rev 2 (adversary review applied)
**Date**: 2026-04-30
**Deciders**: Architect role; implementer to execute after rev-2 adversary sign-off
**Context**: Phase 17 items 2 + 3 follow-ups from
`specs/implementation/phase-17-cross-node-followups.md`
**Related ADRs**: ADR-004 (schema versioning), ADR-011 (crypto-shred
TTL), ADR-016 (backup/DR), ADR-024 (device management + capacity),
ADR-026 (Raft topology), ADR-029 (raw block allocator),
ADR-030 (small-file placement), ADR-032 (async gateway ops),
ADR-036 (LogOps shard management)

## Revision history

- **rev 1** (2026-04-30, commit `a08e479`): initial draft.
- **rev 2** (2026-04-30, this revision): incorporates adversary
  findings F-1..F-7 from
  `specs/findings/adr-040-adversary-review.md`.
  - F-1 → §D5 specifies the transient-vs-permanent skip algorithm,
    new invariant **I-CP6**.
  - F-2 → §D5 drops `last_applied_log_index`; only
    `meta.last_applied_seq: SequenceNumber` is stored.
  - F-3 → §D6.3 specifies the gap-detection mechanism (sequence-
    comparison; no new `LogOps` API needed).
  - F-4 → §D7 picks "configurable retry + observability" over
    write-through on the leader; adds two metrics.
  - F-5 → new §D8.1 places `PersistentStoreError` in the error
    taxonomy.
  - F-6 → new §D10 specifies the observability surface
    (9 metrics).
  - F-7 → new §D11 explicitly scopes persistence to
    `compositions` only; `namespaces` + `multiparts` stay
    in-memory.
  - F-8..F-17 acknowledged as Medium / Low — addressed inline
    during implementation review (auditor + post-impl adversary).

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
change has been durably committed AND the state change either
applied successfully or is intentionally a no-op for this
operation type.**

The "intentionally a no-op" qualifier closes adversary finding
F-1 (Phase 17 item 1's hydrator advanced `last_applied` past
failed `update_at` calls, losing the Update permanently with
persistence). The transient-vs-permanent skip algorithm in §D5.1
implements the qualifier.

Per hydrator poll, the durable persistence point:

```
begin_write()
  for each delta in this poll's batch (advance scope, see D5.1):
    apply (insert / update / delete in COMPOSITIONS table)
  meta.put("last_applied_seq", advanced_to_seq)   # SequenceNumber, see F-2
commit()       <-- single fsync
update LRU cache atomically with the lock window (D3)
```

Note: only `last_applied_seq` (the per-shard delta sequence) is
persisted. The openraft `log_index` is **not** stored — the
hydrator reads via `LogOps::read_deltas(SequenceNumber)` and has
no occasion to convert. Any future ADR that needs log-index
alignment (e.g. snapshot coordination — D6.2) introduces the
mapping there, not here.

If the process crashes between `apply` and `commit`, redb's
journaling rolls everything back; on restart, `last_applied_seq`
reads the value from before the failed batch. If the process
crashes between `commit` and the LRU update, the next read pays a
redb miss and re-hydrates the cache — correct, just slightly slow.

Bound on lost work: at most one batch (default 1000 deltas) per
crash. The hydrator picks up where it left off on restart.

#### D5.1. Transient skip vs permanent skip

A delta in the poll batch falls into one of three buckets when
the hydrator tries to apply it:

| Outcome | What happened | Action |
|---|---|---|
| **Applied** | apply method returned Ok | advance `last_applied_seq` past this delta |
| **Permanent skip** | the delta is structurally un-applyable: bad payload length, unknown OperationType, decode error, etc. | advance `last_applied_seq` past it; increment `kiseki_composition_hydrator_skip_total{reason=...}` at warn level |
| **Transient skip** | apply returned a `MaybeRecoverable` error (e.g. `update_at` got `CompositionNotFound`, `create_at` got `NamespaceNotFound`) | DO NOT advance past this delta on this poll; bump the per-delta retry counter; return early from this poll |

A transient skip blocks all later deltas in the same poll
batch. The next poll re-reads from this delta's sequence and
retries. The retry counter is in-memory (it doesn't have to
survive a crash — on restart, the loop just retries from
durable `last_applied_seq + 1` and the counter resets). When the
counter exceeds `KISEKI_HYDRATOR_TRANSIENT_RETRIES` (default
**100**, ≈ 10 s at 100 ms poll cadence), the hydrator escalates:

- log at error with the delta's seq + tenant + comp_id;
- emit `kiseki_composition_hydrator_stalled = 1`;
- advance past the delta (refusing forever blocks worse failure
  modes than losing one record);
- increment `kiseki_composition_hydrator_skip_total{reason="exhausted_retries"}`.

Operators alarming on the stalled gauge or the
`exhausted_retries` counter can investigate. Common cause:
namespace not yet replicated to this node (Phase 18 territory) —
the alarm and the metric label make this diagnosable.

This is the algorithmic shape; the implementer picks the exact
typed-error mapping. The minimum surface:

```rust
pub enum HydratorOutcome {
    Applied,
    PermanentSkip { reason: &'static str },
    TransientSkip { reason: &'static str },
}
```

`update_at` and `create_at` return enough information for the
hydrator to map to the right variant. See I-CP6.

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

**D6.3 Self-defense — gap detection without a new `LogOps` API.**

The hydrator detects compaction by inspecting the deltas it
receives, not by querying the log's earliest visible sequence
directly. (Adversary finding F-3: `LogOps` exposes no
`earliest_visible_seq` and adding one is more API surface than
this case justifies.) The detection rule:

After `read_deltas(from = last_applied + 1, to = last_applied + 1000)`:

| Response | Meaning | Action |
|---|---|---|
| Non-empty, first delta's `sequence == last_applied + 1` | Normal advance — no gap. | Apply per §D5.1. |
| Non-empty, first delta's `sequence > last_applied + 1` | **Gap.** Compaction has eaten the deltas in between. | **Halt mode** (see below). |
| Empty AND `shard_health(shard).tip > last_applied` | **Gap.** Tip is past us but no deltas are visible — they were compacted. | **Halt mode**. |
| Empty AND `tip <= last_applied` | Steady state — no new deltas yet. | Sleep until next poll. |

**Halt mode**: the hydrator emits one `tracing::error!` per minute
(throttled — don't spam the log), sets the
`kiseki_composition_hydrator_stalled` gauge to 1, and stops
polling for new deltas. It still serves reads from the existing
persistent store; reads of compositions created before the
compaction continue to work, reads of compositions created after
the compaction return 404 (the hydrator can't catch up without
operator intervention).

**Operator recovery procedure** (until D6.2 lands):

1. Stop the kiseki-server process on this node.
2. Delete `KISEKI_DATA_DIR/metadata/compositions.redb` and
   `views.redb`.
3. Start kiseki-server. The hydrator initializes empty stores
   (D9 first-boot path), receives the next openraft snapshot
   (which includes all visible deltas), and re-hydrates from
   scratch.

This procedure has cluster-side impact (the recovering node
returns 404 for cross-node reads during re-hydration) but is
correct. Document as the operational SOP for "a node was offline
long enough for log compaction to outrun it" until the compaction-
aware ADR ships.

**Why this is OK as a precondition for ADR-040 and not a blocker
for the ADR sibling that turns on log compaction:**

- Log compaction is **not** enabled in the codebase today (the
  Raft state machine's snapshot includes all deltas, see
  §D6.1). So halt-mode never fires in steady state.
- When compaction is enabled by a sibling ADR, that ADR's design
  must address the bundle-transfer protocol; halt-mode is the
  conservative "fail loud" stance that lets us turn compaction
  on without first solving D6.2 perfectly.

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
  read on the leader pays the hydrator's poll latency
  (≤ 100 ms baseline) plus apply time (depends on batch size and
  redb commit latency). Phase 16f's gateway-side retry budget
  absorbs this; rev 2 makes the budget configurable and observable
  to address adversary finding F-4.

  - **`KISEKI_GATEWAY_READ_RETRY_BUDGET_MS`** (env, default 1000):
    bounds the read-path retry on `CompositionNotFound`. Operators
    on slow disks or under load can tune up; the default fits
    well-provisioned NVMe.
  - **`kiseki_gateway_read_retry_total`** (counter, label `node_id`):
    every read that exited the retry loop with a hit. Steady-state
    rate is ~1× the cross-gateway read rate.
  - **`kiseki_gateway_read_retry_exhausted_total`** (counter, label
    `node_id`): every read that hit the budget without resolving.
    A non-zero rate means the budget is too tight for the current
    hydrator latency. Operators alarm on this and either bump the
    budget or investigate hydrator stall (which has its own
    metrics — see §D10).

  Alternative considered: write-through on the leader (gateway
  also writes to local persistent store on emit-success).
  **Rejected** because (a) it's redundant — the hydrator does it
  ~100 ms later anyway, (b) it introduces a partial-success
  failure mode (emit succeeds, local write fails — which does the
  client see? the cluster says "yes" but the leader's local state
  says "no"), (c) it needs rollback on emit failure (mirroring
  ADR-032's rollback for in-memory state), adding complexity.
  Sticking with eventual-consistency-plus-bounded-retry preserves
  the single-consistency-model property: every node sees the
  composition once the hydrator has applied the delta.

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

### D8.1. Typed errors (`PersistentStoreError`)

Closes adversary finding F-5: the rev 1 ADR named `SchemaTooNew`
without placing it in the error taxonomy.

A new module `kiseki_composition::persistent::error` introduces:

```rust
pub enum PersistentStoreError {
    /// I/O against the underlying redb (open, read, write, fsync).
    Io(io::Error),
    /// The on-disk record carries a schema_version this binary
    /// doesn't know how to decode. Surfaced as "binary too old".
    SchemaTooNew { found: u8, supported: u8 },
    /// Postcard decode failure — payload bytes don't match the
    /// declared schema_version's struct shape.
    Decode(String),
    /// A persistent-store call delegated to an in-memory
    /// `CompositionStore` operation (e.g. `create_at` rule
    /// validation) and that returned a domain error.
    Composition(#[from] CompositionError),
    /// redb commit failed — surfaced separately so operators can
    /// distinguish from raw I/O.
    Commit(String),
}
```

**Gateway boundary.** `GatewayError` does NOT gain a new variant.
The persistent-store layer maps every variant to
`GatewayError::Upstream(format!("..."))` and increments
`kiseki_composition_decode_errors_total{kind=...}` (see §D10).
Operators get the metric label for alarm routing; the error
string carries the human-readable detail. This keeps
`GatewayError` stable; the type-discriminator lives in metrics.

`error-taxonomy.md` is updated to list `PersistentStoreError` in
the persistent-store row, mapped to `GatewayError::Upstream`.

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

### D10. Observability surface

Closes adversary finding F-6: rev 1 was silent on metrics, leaving
operators no way to diagnose F-1 (silent skip), F-4 (RYW retry
budget exhaustion), or F-8 (commit failure stalls). The
implementer adds these counters / gauges / histograms to
`crates/kiseki-server/src/metrics.rs` alongside the existing
`KisekiMetrics::fabric` block (Phase 16 pattern):

| Metric | Type | Labels | Purpose |
|---|---|---|---|
| `kiseki_composition_redb_size_bytes` | gauge | — | disk-fill alarm; ties to ADR-024 budget |
| `kiseki_composition_count` | gauge | — | growth tracking |
| `kiseki_composition_lru_capacity` | gauge | — | sizing context |
| `kiseki_composition_lru_hit_total` | counter | — | tune cache size |
| `kiseki_composition_lru_miss_total` | counter | — | tune cache size |
| `kiseki_composition_lru_evicted_total` | counter | — | thrashing detection |
| `kiseki_composition_hydrator_apply_duration_seconds` | histogram | — | RYW retry budget rationale (F-4) |
| `kiseki_composition_hydrator_last_applied_seq` | gauge | `shard_id` | replication lag |
| `kiseki_composition_hydrator_skip_total` | counter | `reason` (∈{`bad_payload`,`unknown_op`,`exhausted_retries`,`schema_too_new`,`decode`}) | catches F-1 |
| `kiseki_composition_hydrator_stalled` | gauge | — | halt-mode signal (D5.1, D6.3) |
| `kiseki_composition_redb_commit_errors_total` | counter | — | catches F-8 (disk full, I/O) |
| `kiseki_composition_redb_read_txn_active` | gauge | — | reader contention (F-11) |
| `kiseki_composition_decode_errors_total` | counter | `kind` (∈{`schema_too_new`,`postcard`,`length`}) | F-5 typed-error visibility |

Plus the gateway-side retry metrics from §D7:

| Metric | Type | Labels | Purpose |
|---|---|---|---|
| `kiseki_gateway_read_retry_total` | counter | — | retry rate baseline |
| `kiseki_gateway_read_retry_exhausted_total` | counter | — | F-4 alarm |

The same shape applies to the persistent ViewStore (replace
`composition` with `view` in the metric names). The implementer
factors a small helper to avoid duplication.

### D11. Persistence scope — only `compositions`

Closes adversary finding F-7: rev 1 said "make CompositionStore
persistent" without distinguishing the three independent maps
the struct holds.

Only the **`compositions: HashMap<CompositionId, Composition>`**
map is moved to redb. The other two stay in-memory:

- **`namespaces: HashMap<NamespaceId, Namespace>`** stays
  in-memory, recreated on every boot. The bootstrap "default"
  namespace is installed by `runtime.rs` (Phase 16f §D6.3
  fix — installed on every node, not gated on
  `cfg.bootstrap`). Tenant-created namespaces aren't replicated
  yet; that's Phase 18 territory and gets its own ADR with its
  own replicate-and-persist path.
- **`multiparts: HashMap<String, (MultipartUpload, NamespaceId)>`**
  stays in-memory. In-flight multipart uploads are dropped on
  restart, consistent with current S3 semantics: an S3 client
  treats a server-side state loss as an aborted upload and
  retries (the `Initiate Multipart Upload` returns a fresh
  `upload_id` on retry). Persisting them across restart
  resurrects state from a different client session — the wrong
  semantics.

For ViewStore: the analogous decision is "all of it persists"
since views aren't transient state. Architect's call; implementer
follows the same shape (one redb, two tables).

## Invariants

These get added to `specs/invariants.md` (status `Proposed` until
the implementation lands, then `Confirmed`):

- **I-CP1**: A persistent `CompositionStore` advances
  `meta.last_applied_seq` only as part of the same redb transaction
  that applies the corresponding state changes (or no-ops, see
  I-CP6). Crash between batches loses at most one batch's worth of
  work; on restart the hydrator resumes from the durably-committed
  `last_applied_seq + 1`.

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
  `meta.last_applied_seq`, the hydrator detects the gap (see
  §D6.3's sequence-comparison rule) and enters halt mode: emits
  one throttled `tracing::error!` per minute, sets
  `kiseki_composition_hydrator_stalled = 1`, and stops polling.
  Existing reads continue to be served from the persistent store
  (compositions created before compaction stay reachable). The
  operator's recovery action is to drop the node's metadata redbs
  and restart; the persistent store re-hydrates from the next
  openraft snapshot. Until the deferred snapshot-bundle ADR lands,
  this is the only correct behavior.

- **I-CP6**: The hydrator advances `last_applied_seq` past delta
  `D` if and only if one of:
  (a) the apply method returned `Ok` (state mutation succeeded);
  (b) `D` is structurally un-applyable (bad payload length, unknown
       OperationType, decode error) — this is a *permanent skip*,
       advances + warns + emits
       `kiseki_composition_hydrator_skip_total{reason}`;
  (c) `D` is intentionally a no-op for the hydrator
       (Rename / SetAttribute / Finalize today) — silent advance.

  A *transient skip* (e.g. `update_at` returning
  `CompositionNotFound`, `create_at` returning
  `NamespaceNotFound`) does **not** advance — the hydrator
  retries on the next poll. After
  `KISEKI_HYDRATOR_TRANSIENT_RETRIES` consecutive transient skips
  (default 100, ≈ 10 s at 100 ms poll), the skip is promoted to
  permanent (case b) with `reason="exhausted_retries"` and the
  hydrator alarms via `kiseki_composition_hydrator_stalled`.

  This invariant addresses adversary F-1: under in-memory state
  the no-advance-on-error semantic was implicit and self-healing;
  under persistence, the hydrator must distinguish so a transient
  upstream condition (e.g. namespace not yet replicated to this
  node) doesn't permanently lose deltas.

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

**Rev 1 review** completed at
`specs/findings/adr-040-adversary-review.md` (commit `a6eec3c`).
Verdict: 3 Critical + 4 High findings (F-1..F-7) blocked
implementation pending revision. **Rev 2 (this revision)**
addresses all seven inline:

| Finding | Severity | Resolution |
|---|---|---|
| F-1 (silent advance past failed Updates) | Critical | §D5.1 transient/permanent skip algorithm; new I-CP6 |
| F-2 (SequenceNumber vs log_index conflation) | Critical | §D5 drops `last_applied_log_index` |
| F-3 (no API for earliest_visible_seq) | Critical | §D6.3 sequence-comparison gap detection (no new API) |
| F-4 (RYW retry budget invisible) | High | §D7 configurable budget + 2 metrics |
| F-5 (typed error placement) | High | §D8.1 `PersistentStoreError` enum |
| F-6 (no observability) | High | §D10 13 metrics specified |
| F-7 (namespaces/multiparts scope) | High | §D11 only `compositions` is persisted |

Six Medium / four Low findings (F-8..F-17) are deferred to
implementation review (auditor + post-impl adversary pass) per
the rev 1 reviewer's recommendation; they don't block the
architect-to-implementer handoff.

**Standing implementation-review concerns** (these are guidance
for the auditor + post-impl adversary, not blockers for the
architect handoff):

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
