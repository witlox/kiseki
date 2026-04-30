# ADR-040 Adversary Review

**Reviewer**: Adversary role
**Date**: 2026-04-30
**Subject**: `specs/architecture/adr/040-persistent-metadata-stores.md` (Proposed)
**Mode**: Architecture mode (no code yet)
**Verdict**: **Conditionally accept** — 3 Critical findings (F-1, F-2, F-3) must be resolved in the ADR before implementation; High findings should be addressed in the same revision; Medium / Low can be opened as follow-up tickets if the architect prefers.

---

## Summary

| Severity | Count | IDs |
|---|---|---|
| Critical | 3 | F-1, F-2, F-3 |
| High | 4 | F-4, F-5, F-6, F-7 |
| Medium | 6 | F-8, F-9, F-10, F-11, F-12, F-13 |
| Low | 4 | F-14, F-15, F-16, F-17 |

The basic architecture (redb + LRU + single-writer hydrator + atomic
`last_applied + state`) is sound. The Critical findings are
data-loss / divergence risks that the current ADR phrasing actively
hides. The High findings are gaps a careful implementer would have
to fill in by guessing — exactly what the ADR's job is to prevent.

---

## Critical findings

### Finding F-1: Failed Update applications silently advance `last_applied_seq`, losing the update across restart

**Severity**: Critical
**Category**: Correctness > Failure cascades
**Location**: ADR-040 §D5 (atomic last_applied + state); already manifest in `crates/kiseki-composition/src/hydrator.rs` line ~118 (`update_at` error path)
**Spec reference**: I-CP1 ("a crash between batches loses at most one batch")

**Description**: Phase 17 item 1's `update_at` returns
`CompositionError::CompositionNotFound` if the comp_id has no prior
Create. The hydrator's loop catches this, logs at debug level, and
**advances `last_applied = delta.header.sequence`** anyway. With
in-memory state this is annoying but recoverable on restart (the
hydrator re-reads from seq=1). With persistence as ADR-040
specifies it, `last_applied_seq` is durably committed; the failed
Update is permanently skipped and the composition stays at its
pre-Update state forever, on this node only. Other nodes that
successfully applied the Update have a different state. **Silent
divergence.**

The "missing Create" condition shouldn't normally occur (deltas are
applied in sequence order), but it does happen during transient
states: the snapshot install replaces `inner.deltas` with a range
that doesn't include the Create that this Update references; or a
corrupted log entry; or — most likely — an operator's mistake of
dropping the metadata redb without dropping the log redb, leaving
last_applied=0 against compositions=[] but the log still has Update
records whose Create predecessors are in chunks the operator
hasn't replayed yet.

**Evidence**: Trace through hydrator.rs lines ~114–127 with a log
that contains `[Create#1 of comp_X, Update#2 of comp_X]` against an
empty CompositionStore where the Create somehow didn't get
processed (e.g. Composition tenant_id refers to a tenant whose
namespace isn't registered yet). The Create returns
`NamespaceNotFound`; the loop logs and advances. The Update
returns `CompositionNotFound`; the loop logs and advances.
`last_applied_seq=2` is committed. The composition is **lost**
permanently — the hydrator will never re-process seq=1.

**Suggested resolution**:

1. Distinguish "transient skip" from "permanent skip" at the
   hydrator level. A transient skip (e.g. namespace not yet
   registered) must NOT advance `last_applied_seq`; the next poll
   retries.
2. A permanent skip (e.g. unknown OperationType byte) advances and
   logs a warning.
3. Make `update_at(...)` return a typed error so the hydrator can
   distinguish "composition was deleted earlier in this batch"
   (advance) from "Update arrived without a prior Create" (don't
   advance, retry next poll, alarm if persists for >N polls).
4. Add an invariant: I-CP6 — the hydrator advances
   `last_applied_seq` past delta `D` iff every state mutation
   implied by `D` has been applied or D is intentionally a no-op
   (Rename, SetAttribute, Finalize).

---

### Finding F-2: `last_applied_seq` (SequenceNumber) and openraft `log_index` (LogIndex) are conflated in §D5

**Severity**: Critical
**Category**: Correctness > Semantic drift
**Location**: ADR-040 §D5

**Description**: §D5's pseudocode shows
`meta.put("last_applied_log_index", entry.log_index)` alongside
`meta.put("last_applied_seq", new_seq)`. These are two different
namespaces:

- `SequenceNumber` is the per-shard delta sequence (`Delta::header.sequence`),
  starts at 1, monotone within a shard.
- `log_index` is the openraft log index, monotone across all entries
  in the Raft log including membership and blank entries.

The hydrator reads via `LogOps::read_deltas(from: SequenceNumber, to: SequenceNumber)`
— it has no notion of log_index. The ADR's mention of
`last_applied_log_index` is dead weight at best and load-bearing
at worst (a future implementer might decide to use log_index for
snapshot-coordinate alignment, then realize the hydrator has no
way to convert).

**Evidence**: `crates/kiseki-log/src/traits.rs:126` —
`read_deltas(ReadDeltasRequest { shard_id, from: SequenceNumber, to: SequenceNumber })`. No log-index variant exists.

**Suggested resolution**: Drop `last_applied_log_index` from §D5.
Specify only `meta.last_applied_seq: SequenceNumber`. If a future
ADR (D6.2 / log compaction) needs log-index alignment, that ADR
adds the conversion API.

---

### Finding F-3: §D6.3's self-defense check has no API to call

**Severity**: Critical
**Category**: Correctness > Missing primitive
**Location**: ADR-040 §D6.3

**Description**: §D6.3 says "if `last_applied_seq < earliest_visible_seq`,
the hydrator emits `tracing::error!` and stops polling". But
**`LogOps` exposes no `earliest_visible_seq` API** today. The hydrator
calls `read_deltas(from=last_applied+1, to=last_applied+1000)`. If
the log was truncated past `last_applied`, the response is just an
empty `Vec<Delta>` (or worse: deltas starting at
`new_log_start`, which the hydrator naively applies as if they
extended the previous range). There's no way to detect compaction
without a new API.

**Evidence**: Searched `crates/kiseki-log/src/traits.rs` — only
`read_deltas`, `append_delta`, `append_chunk_and_delta`,
`shard_health` (which exposes `tip` but not floor), `truncate_log`,
`compact_shard`. None expose the earliest visible sequence.

**Suggested resolution**:

1. Either add `LogOps::earliest_visible_seq(shard_id) -> SequenceNumber`
   as a precondition for ADR-040 (not deferred to "future ADR-XXX")
   — the self-defense behavior in §D6.3 needs it now.
2. Or (cheaper, immediate): the hydrator detects compaction by
   comparing the first delta returned by `read_deltas(from=last_applied+1, to=...)` —
   if its sequence is `> last_applied + 1`, there's a gap, treat
   as compaction. This works without an API addition. The ADR
   should specify which strategy.

Either way, §D6.3 must say *how* the hydrator detects the
condition, not just what it does after detecting it.

---

## High findings

### Finding F-4: Read-after-write on the leader regresses from "instant" to "≤ hydrator poll latency"

**Severity**: High
**Category**: Correctness > Failure cascades
**Location**: ADR-040 §D7

**Description**: §D7 says "the leader's gateway doesn't write
directly to its persistent store; it goes through the same
hydrator path as followers." This means a PUT-then-GET on the
leader pays at minimum the hydrator's poll interval (default 100
ms) plus apply time. Phase 16f's gateway-side 1-second retry
absorbs this — but only if the hydrator stays within its budget.
Under load (slow disk, contended LRU lock, large batches), p99
poll-to-commit can exceed 1 s and the retry fires AssertionError.

The Phase 16f retry was designed for **cross-node** race
absorption. ADR-040 silently extends it to **same-node**
read-after-write. Different problem, same retry budget. Operators
hitting a workload that pushes hydrator latency past 1 s will see
their leader-PUT-then-leader-GET sequences fail intermittently and
won't have a hint about why (the retry budget is hard-coded).

**Suggested resolution**: Two paths, architect picks one:

1. **Write-through on the leader.** The gateway's emit-success
   path also writes to the local persistent store. Yes, it
   doubles the write work (hydrator will re-apply), but `create_at`
   is idempotent. Adds explicit RYW. Cost: ~5x faster RYW under
   contention, at the cost of one extra redb txn per leader PUT.
2. **Make the retry budget configurable + observable.** Add
   `KISEKI_GATEWAY_READ_RETRY_BUDGET_MS` env (default 1000),
   expose `kiseki_gateway_read_retry_total` (counter) and
   `kiseki_gateway_read_retry_exhausted_total` (counter). When
   `_exhausted` rises, operators know to bump the budget or
   investigate hydrator latency.

The ADR should pick one and specify it. My adversary stance:
option 2 is honest (eventual consistency stays eventually
consistent; operators get visibility); option 1 quietly slips into
"strong consistency on the leader, eventual on followers" which is
a different model and has its own corner cases (leader changes,
emit succeeds but local write fails, etc.).

---

### Finding F-5: `SchemaTooNew` is named in §D8 but not placed in the error taxonomy

**Severity**: High
**Category**: Correctness > Spec compliance
**Location**: ADR-040 §D2, §D8

**Description**: §D2 and §D8 reference a `SchemaTooNew` typed error
returned by decoders, but the ADR doesn't say where it lives in
the error taxonomy. Today's `CompositionError` enum (in
`crates/kiseki-composition/src/error.rs`) has variants like
`CompositionNotFound`, `NamespaceNotFound`,
`ReadOnlyNamespace`, etc. Adding `SchemaTooNew` there pollutes
the trait surface with a storage-layer concern.

A persistent-store-specific error type makes more sense:

```rust
pub enum PersistentStoreError {
    Io(io::Error),
    SchemaTooNew { found: u8, supported: u8 },
    Decode(postcard::Error),
    Composition(CompositionError),
}
```

The gateway's error path needs to map this to a typed
`GatewayError` variant rather than the current
`GatewayError::Upstream(string)`, otherwise operators see
"upstream error: schema too new: found=2 supported=1" and can't
distinguish between "binary too old" and "data corrupted" in
metric labels.

**Suggested resolution**: Add §D8.1 to the ADR specifying:
- `kiseki_composition::PersistentStoreError` enum (or similar
  module path).
- The `GatewayError` variant that wraps it
  (`GatewayError::PersistentStore(PersistentStoreError)`).
- error-taxonomy.md gets the new variants.

---

### Finding F-6: No observability surface specified — operators can't diagnose or tune

**Severity**: High
**Category**: Robustness > Observability gaps
**Location**: ADR-040 (entire — silent on metrics)

**Description**: The ADR specifies behavior but not how to see it.
Without metrics, an operator hitting F-4 (RYW retry exhausted) or
F-1 (state divergence) has no instrumentation to localize the
problem. The Phase 16f hydrator already logs at info on
`applied > 0`; that's not actionable.

**Suggested resolution**: Add §D10 specifying the metrics surface:

| Metric | Type | Why |
|---|---|---|
| `kiseki_composition_redb_size_bytes` | gauge | disk-fill alarm |
| `kiseki_composition_count` | gauge | growth tracking |
| `kiseki_composition_lru_capacity` | gauge | sizing context |
| `kiseki_composition_lru_hit_total` / `_miss_total` | counter | tune cache |
| `kiseki_composition_lru_evicted_total` | counter | thrashing detection |
| `kiseki_composition_hydrator_apply_duration_seconds` | histogram | latency budget for F-4 |
| `kiseki_composition_hydrator_last_applied_seq{shard}` | gauge | replication lag |
| `kiseki_composition_hydrator_skip_total{reason}` | counter | catches F-1 |
| `kiseki_composition_redb_commit_errors_total` | counter | catches F-8 |

These should be added under the existing
`crates/kiseki-server/src/metrics.rs` `KisekiMetrics` struct
following the same Phase 16's `metrics::FabricMetrics` pattern.

---

### Finding F-7: `CompositionStore`'s `namespaces` and `multiparts` fields aren't addressed by the ADR

**Severity**: High
**Category**: Correctness > Implicit coupling
**Location**: ADR-040 (entire); `crates/kiseki-composition/src/composition.rs:229–230`

**Description**: `CompositionStore` holds three independent maps:
`compositions`, `namespaces`, `multiparts`. ADR-040 talks about
making "the CompositionStore" persistent without distinguishing
between them. An implementer might:

- Persist all three (extra work, breaks bootstrap-namespace and
  in-flight-multipart semantics);
- Persist only compositions (correct intent? unclear);
- Persist compositions + namespaces (breaks because tenant-created
  namespaces aren't replicated yet — they'd appear stale on
  followers post-Phase-18).

Multipart uploads have their own "in-flight" semantics: a server
crash mid-upload should drop the partial state, not resume it. If
multiparts persist, a node restart could resurrect a partial
upload from a different client's session.

**Suggested resolution**: Add §D11 specifying:

- Only `compositions` is persisted by ADR-040.
- `namespaces` stays in-memory; the bootstrap "default" is
  recreated on every boot (Phase 16f §D6.3 fix). Tenant-created
  namespaces are Phase 18 territory and will get their own
  replicate-and-persist path.
- `multiparts` stays in-memory; in-flight uploads are lost on
  restart (consistent with current semantics — an S3 client
  retries). Document this explicitly.

---

## Medium findings

### Finding F-8: redb commit-failure path (disk full, etc.) not specified

**Severity**: Medium
**Category**: Robustness > Resource exhaustion
**Location**: ADR-040 §D5

**Description**: `redb::WriteTransaction::commit()` returns
`Result<(), CommitError>`. CommitError can be triggered by
out-of-space, I/O error, or a bug. The hydrator's `poll()` would
return without advancing `last_applied`. Next poll re-reads the
same deltas — the fast-loop is bounded by the 100 ms sleep.
Effectively the hydrator stalls until the disk recovers. No alarm,
no backoff, no observability of how many polls have been blocked.

**Suggested resolution**: §D5 should specify:
- Commit-error handling: log at warn, increment
  `kiseki_composition_redb_commit_errors_total`, retry on next
  poll.
- After N consecutive failures (default 60 = 6 minutes at 100 ms
  poll), promote to error log + alarm metric (gauge:
  `kiseki_composition_hydrator_stalled = 1`).
- Operator action: free disk space, hydrator recovers automatically.

Cross-reference ADR-024's metadata budget (the redb size should
trigger the soft/hard limit alarms before commit-fails actually
happen).

---

### Finding F-9: redb-corruption-on-open recovery path not specified

**Severity**: Medium
**Category**: Robustness > Error handling quality
**Location**: ADR-040 §D9

**Description**: §D9 covers first-boot (path doesn't exist → init).
It does NOT cover: redb file exists but can't be opened (file
truncated, header corrupted, schema_version key missing). What
should the server do?

Three choices:
- **Refuse to start** with a clear error → safest; operator
  diagnoses.
- **Auto-rebuild** by deleting the corrupt file and rehydrating →
  fast recovery; risks masking real corruption.
- **Quarantine** (rename to `.corrupt`, init fresh, log error).

**Suggested resolution**: Pick one, specify in §D9. My adversary
preference: refuse-to-start by default, with
`KISEKI_COMPOSITION_AUTO_REBUILD=true` opt-in for production
clusters that prefer availability over forensics. Same pattern as
the Phase 16 plaintext-NFS opt-in (env-gated).

---

### Finding F-10: Per-shard hydrator architecture deferred but blocks day-one design

**Severity**: Medium
**Category**: Correctness > Implicit coupling
**Location**: ADR-040 §"Open questions / future work" item 4

**Description**: §D7 says "one hydrator per node". §"Open questions"
says "per-shard hydrator (when ADR-033 multi-shard topology lands
in production): one hydrator per shard". These are different
architectures and the implementer has to pick one to write.

If the implementer writes single-shard now and multi-shard later,
the runtime spawn changes from a single `tokio::spawn(...)` to a
`Vec<JoinHandle>` orchestrator. The persistent store's `meta` keys
also change from `last_applied_seq` to `last_applied_seq.<shard_id>`.
That's a redb format change.

**Suggested resolution**: Design for multi-shard from day one.
Single hydrator instance now, but its API takes a `Vec<ShardId>`
and the meta key is namespaced as `last_applied_seq.<shard_uuid>`
from the start. When ADR-033 multi-shard ships, the runtime adds
shards to the existing hydrator's tracking list — no redb format
change. Same pattern as the view stream processor's
`tracked_views: Vec<ViewId>` field.

---

### Finding F-11: Read-contention on redb during writer-pending state not quantified

**Severity**: Medium
**Category**: Correctness > Concurrency
**Location**: ADR-040 §D7

**Description**: §D7 mentions "multiple concurrent reads serialize
on the LRU mutex *only* during the brief lookup; cache hits don't
touch redb". Cache misses do. Under cold-cache / churning workload
(initial hydration, large compositions), every gateway read is a
miss → redb read txn. redb's single-writer model lets writers wait
for outstanding readers (or vice versa, depending on isolation
config).

In the worst case, a hydrator commit waits for N concurrent
gateway reads to finish their txns. With many readers, the
hydrator's poll-to-commit stretches arbitrarily, feeding back into
F-4 (RYW retry exhaustion).

**Suggested resolution**: §D7 should specify redb's transaction
isolation level (snapshot — readers don't block writers in MVCC
mode). Verify this is the redb default (it is, but document it).
Add a metric `kiseki_composition_redb_read_txn_active` (gauge) so
operators can see contention.

---

### Finding F-12: ADR-024 metadata-budget integration missing

**Severity**: Medium
**Category**: Robustness > Resource exhaustion
**Location**: ADR-040 (entire)

**Description**: ADR-030 §1 defines a metadata budget for inline
content storage on the system disk. ADR-024 specifies per-node
capacity tracking with `KISEKI_META_SOFT_LIMIT_PCT` (50%) and
`KISEKI_META_HARD_LIMIT_PCT` (75%). The new
`metadata/{compositions,views}.redb` files consume this budget but
the ADR doesn't update the tracking.

A 1-billion-composition cluster eating 280 GB of metadata redb
silently overflows the existing 75% hard-limit alarm — the alarm
was sized for inline content, not for compositions+views.

**Suggested resolution**: Add §D12 explicitly including the new
redbs in the metadata-budget calculation. Cross-reference ADR-024
and ADR-030. Specify that
`KISEKI_META_HARD_LIMIT_PCT` accounts for sum of:
- Raft log redb
- inline-content redb
- compositions redb (new)
- views redb (new)

---

### Finding F-13: ADR-016 backup integration missing

**Severity**: Medium
**Category**: Robustness > Audit / data preservation
**Location**: ADR-040 (entire)

**Description**: ADR-016 specifies backup + DR for kiseki. The
backup manager bundles persistent state for off-cluster archival.
ADR-040 introduces two new persistent stores but doesn't say
they're in the backup bundle.

If a cluster is restored from a backup that doesn't include the
metadata redbs, every node bootstraps with empty stores and
re-hydrates from the Raft log. That works *if* the Raft log was
also backed up and contains the full delta history. If openraft
log compaction is enabled (D6.2's future world), the post-restore
cluster can't re-hydrate compositions older than the snapshot
point. Data loss without a corresponding metadata-redb backup.

**Suggested resolution**: Add §D13 specifying that
`PersistentCompositionStore::path()` and
`PersistentViewStore::path()` are exposed and the backup manager
includes both files in its bundle. Cross-reference ADR-016.

---

## Low findings

### Finding F-14: comp_id enumeration via tenant-mismatch errors

**Severity**: Low (pre-existing, not introduced by ADR-040)
**Category**: Security > Tenant isolation
**Location**: ADR-040 §"Adversary review" (incomplete)

**Description**: A tenant probing arbitrary comp_ids gets
`tenant mismatch` (existence-confirming error) vs `not found`
(non-existence). Pre-existing in the in-memory store; ADR-040
doesn't make it worse but doesn't address it either. Persistent
storage means comp_ids live longer (across restarts), making
enumeration over time more durable.

**Suggested resolution**: One-line note in §"Open questions"
referencing this as a known issue with its own future ADR for
constant-time tenant-existence response.

---

### Finding F-15: Crypto-shred zombie compositions persist across reboots

**Severity**: Low
**Category**: Security > Crypto-shred
**Location**: ADR-040 (entire); cross-ref ADR-011

**Description**: When a tenant's KEK is rotated/destroyed
(crypto-shred per ADR-011), referenced chunks become unreadable.
The `Composition` records that reference them stay alive — they
fail decrypt at read time but exist as records. With persistence,
zombie compositions survive restart. Same behavior as in-memory
but more visible.

**Suggested resolution**: One-line note in §"Open questions" that
crypto-shred + persistence interaction is unchanged from in-memory
semantics; cross-reference ADR-011.

---

### Finding F-16: ADR-029 cross-reference missing

**Severity**: Low
**Category**: Correctness > Spec compliance
**Location**: ADR-040 §D1

**Description**: ADR-029 (raw block allocator) requires that all
filesystem-managed metadata sits on the system disk
(`KISEKI_DATA_DIR`), not on raw block devices. ADR-040's
`metadata/` directory respects this implicitly, but the ADR
doesn't cite ADR-029.

**Suggested resolution**: One-line cross-reference in §D1.

---

### Finding F-17: "Snapshot" terminology used loosely for three different things

**Severity**: Low
**Category**: Correctness > Semantic drift
**Location**: ADR-040 §D6

**Description**: §D6 uses "snapshot" to mean (1) the openraft
RaftSnapshotBuilder output, (2) the state-machine's
`ShardSnapshot` struct serialization, and (3) hypothetical bundled
redb files in the future D6.2 world. The reader has to track
which is which from context.

**Suggested resolution**: §D6 explicitly defines the three terms
on first use. e.g.: "Raft snapshot (openraft-level)", "state-
machine snapshot (the `ShardSnapshot` JSON blob)", and
"metadata-store bundle (the future D6.2 sidechannel transfer)".

---

## Recommendation

The ADR is structurally sound but the spec gaps in F-1, F-2, F-3,
F-4, F-5, F-6, F-7 are load-bearing for the implementer. Specific
recommendations for the architect's revision:

1. **Resolve F-1 by adding I-CP6 (or amending I-CP1)** distinguishing
   transient skip (don't advance) from permanent skip (advance +
   warn). The hydrator code in commit `7ef59b1` should be revised
   in the same PR that lands the persistent store.
2. **Resolve F-2 by deleting `last_applied_log_index`** from §D5.
3. **Resolve F-3 by either adding `LogOps::earliest_visible_seq`
   as a precondition, or specifying the gap-detection
   workaround**.
4. **Resolve F-4 by picking one of the two RYW paths** (write-
   through vs configurable retry+observability).
5. **Add §D8.1** for the typed-error placement (F-5).
6. **Add §D10** for observability (F-6).
7. **Add §D11** for `namespaces` / `multiparts` scope (F-7).

After the architect amends the ADR, this adversary review can sign
off and the implementer can proceed. Findings F-8 through F-17 can
be addressed inline during implementation review (auditor + my
post-implementation pass) rather than blocking the architect
revision.

**Status**: Block implementation pending architect revision on
F-1..F-7. Re-review after revision.
