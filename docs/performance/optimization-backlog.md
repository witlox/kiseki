# Performance Optimization Backlog

Optimizations discovered during the 2026-05-05 in-process spike but not
shipped. Captured here so if a future change lands in the same code
path, the relevant items can be picked up at the same time without a
fresh investigation.

Cross-reference: `docs/performance/README.md` (top-level perf state,
post-spike numbers) and the analyst handoff at
`specs/escalations/2026-05-05-analyst-handoff-adr-042-native-gateway.md`.

## Already shipped (2026-05-05 spike)

For context — what's already in the tree and what gain it carried:

| # | Change | Crate(s) | Gain |
|---|---|---|---:|
| S1 | `DecryptCache` TTL + Zeroize-on-evict + `wipe_decrypt_cache()` API for crypto-shred signal pathway. F-CC3 contract enforced. | kiseki-gateway | Security, not perf |
| S2 | Dedup short-circuit on writes: gateway calls `try_increment_if_exists(chunk_id)` before sealing — skips HKDF + AEAD + RNG on dedup hits. | kiseki-gateway, kiseki-chunk | Per-PUT HKDF saved on dedup hits |
| S3 | New trait method `try_increment_if_exists` (single critical section vs two-step `refcount` + `increment_refcount`). Implemented on `ChunkStore` and proxied through `ClusteredChunkStore`. | kiseki-chunk, kiseki-chunk-cluster | One round-trip per dedup-hit write instead of two |
| S4 | **Bugfix**: long-standing double-increment of refcount on dedup-hit writes. `ChunkStore::write_chunk` already increments internally on dedup, but the gateway's `else` branch was calling `increment_refcount` again. Fixed via the pre-seal short-circuit; the seal-then-write race-fallback path no longer re-increments. | kiseki-gateway | Correctness |
| S5 | `SyncBridge` (kiseki-chunk async wrapper): swapped `tokio::sync::Mutex` + `spawn_blocking` → `parking_lot::Mutex` + inline call. Unified with the briefly-introduced `FastBridge`. | kiseki-chunk | ~30 % of PUT-path CPU recovered |
| S6 | Namespace metadata cache (`Arc<RwLock<HashMap<NamespaceId, NamespaceMeta>>>`) on `InMemoryGateway`. Lock-free read-only check on the simple non-conditional write path eliminates one mutex round-trip. | kiseki-gateway | One mutex round-trip eliminated per PUT |
| S7 | `Arc<Mutex<CompositionStore>>` (gateway) + `Arc<Mutex<CompositionStore>>` (hydrator): `tokio::sync::Mutex` → `parking_lot::Mutex`. Audit confirmed no `.await` is held inside the lock. The `delete` path was restructured to release the guard across the Raft emit + re-acquire for the local delete. | kiseki-gateway, kiseki-composition | Lock acquisition cost dropped from ~28 % to ~3 % of PUT-path CPU |
| S8 | Promoted `parking_lot::Mutex<ChunkStore>` → `parking_lot::RwLock<ChunkStore>` in `SyncBridge`. Read methods (`read_chunk`, `refcount`, `list_chunk_ids`, `read_fragment`, `list_fragments`, `snapshot_pools`) take a read lock; writes take a write lock. Trait bound tightened to `T: ChunkOps + Send + Sync + 'static`. Was B7. | kiseki-chunk | Read parallelism on mixed workloads |
| S9 | **Correctness fix**: S3 `If-None-Match: *` raced between the conditional check and the create+bind_name. Two concurrent PUTs with the same name + If-None-Match could both succeed before. The conditional check is now folded into the create critical section, restoring atomicity AND eliminating one composition-mutex acquire on the conditional path. Was B6. | kiseki-gateway | Correctness + one fewer lock acquire |
| S10 | Removed `#[tracing::instrument]` from `write` / `read` / `delete` hot-path methods. Phase histograms cover the observability surface; per-call span machinery was a measurable per-op cost on the in-process flamegraph. Was B3. | kiseki-gateway | <5 % per-call CPU |
| S11 | Dropped eager-format `tracing::debug!` calls from per-PUT and per-chunk hot paths (`?chunk_id` in pieces loop, `entry`/`success` markers, `composition created (pre-Raft)`). `tracing::warn!` paths preserved. Was B9. | kiseki-gateway | <2 % per-call CPU |

End state: PUT 20 089 op/s → **82 k op/s** (4× lift, **1.53× read/write ratio** — at the user's 1.5× target).

## Not shipped — performance backlog

If a future change touches any of these areas, consider also picking up
the corresponding item below.

### B1 (largest remaining lift) — `CompositionStore` HashMap → DashMap

**Where**: `kiseki-composition::composition::CompositionStore` (the
`HashMap<CompositionId, Composition>` and the `name_index` map).

**Why**: The post-spike flamegraph shows ~30 % of PUT-path CPU is in
`CompositionStore::create` + downstream HashMap insert. With 16
concurrent writers on a single Mutex-protected HashMap, all inserts
serialize even though the keys never collide (composition_ids are
fresh UUIDs). A sharded HashMap (`DashMap` or per-shard `RwLock<HashMap>`)
lets parallel inserts proceed.

**Expected lift**: +30 % on PUT under contention; pushes us through
100 k op/s in-process floor.

**Effort**: 1 day. **Risk**: Medium.

**Constraints / risks**:
- DashMap's API differs from HashMap in subtle places (e.g. iteration
  doesn't lock the whole map; entry-API guards have different
  drop-order semantics). Every call site needs review.
- The persistent backend (redb-backed `PersistentRedbStorage` from
  ADR-040 rev 3) sits *behind* the in-memory `CompositionStore` and
  has its own write-behind queue. The DashMap change is at the
  `MemoryStorage` layer; the persistent layer's overlay (also a
  `parking_lot::RwLock<HashMap>`) is a separate target — it could
  benefit from the same change.
- The hydrator's `apply_hydration_batch` (`kiseki-composition::hydrator`)
  takes the composition mutex and applies a batch atomically. Sharding
  changes "atomic batch" semantics to "atomic per-key with eventual
  cross-key visibility." Audit the hydrator's invariants (especially
  I-CP1) for whether per-key atomicity is sufficient.

### B2 — Per-thread metric counters

**Where**: `kiseki-gateway::mem_gateway::InMemoryGateway` —
`requests_total: AtomicU64`, `bytes_written: AtomicU64`,
`bytes_read: AtomicU64`, `workflow_ref_writes: [AtomicU64; 3]`.

**Why**: Every PUT does ~6 `AtomicU64::fetch_add` ops on shared
counters. With 16 workers on the same cache line, this causes ping-
pong on the L1 cache line (false sharing). The flamegraph shows it as
distributed cost across many sites; not individually large but
cumulative.

**Expected lift**: <5 % on the in-process driver. Larger on multi-
socket systems where cross-socket atomic ops are slower.

**Effort**: 0.5 day. **Risk**: Low.

**Implementation hint**: replace each `AtomicU64` with
`Arc<[CachePadded<AtomicU64>]>` (one per CPU, indexed by
`thread_local!` or `std::thread::current().id()`); `fetch_add` on the
local slot; metric exporter sums across all slots at scrape time.
The `crossbeam-utils::CachePadded` wrapper is the canonical fix.

### B3 — `#[tracing::instrument]` on hot-path methods ✅ **Picked up 2026-05-05** (S10 above)

Removed from `write` / `read` / `delete` on `InMemoryGateway`. Multipart and admin methods retain instrumentation since their per-op cost dominates the span overhead. Persistent-store methods deferred — measure first if a future flamegraph shows them hot.

### B4 — Pre-allocate Vec capacities on the write path

**Where**: `InMemoryGateway::write` — `let mut landed: Vec<ChunkLanded>`
is `with_capacity(pieces_len)`, but other allocations
(`emit_params.3.clone()` for chunk_ids, `chunk_ids = ...iter().map(...).collect()`)
do fresh allocations per PUT.

**Why**: Allocator pressure under high concurrency.

**Expected lift**: <2 %.

**Effort**: <30 minutes. **Risk**: None.

### B5 — DEK cache (separate ADR-04X)

Captured in `specs/escalations/2026-05-05-analyst-handoff-adr-042-native-gateway.md`
open question 8. Avoid HKDF on every read for the hot-chunk read-
repeat workload (HPC training loops re-reading the same chunks).

**Expected lift**: ~10 % on read-heavy workloads with low chunk
diversity (training datasets), zero on cold reads.

**Effort**: 1–2 days. **Risk**: Medium-High — security-relevant.
Must follow the same discipline as the plaintext cache (Zeroize,
TTL, crypto-shred-wipe signal). Requires its own ADR amending
ADR-002 / ADR-011.

### B6 — Two compositions.lock() acquisitions per PUT — collapse to one ✅ **Picked up 2026-05-05** (S9 above)

Side benefit: the conditional check is now race-free against concurrent writers to the same name (S3 If-None-Match: * atomicity restored). The previous code had a real race window between the check and the create.

### B7 — Read-mostly RwLock on the chunk store ~~?~~ ✅ **Picked up 2026-05-05** (S8 above)

### B8 — Composition record encoding lazy on persistent backend

**Where**: `kiseki-composition::persistent::redb::PersistentRedbStorage::put`
(and the write-behind queue's `commit_snapshot_to_redb`).

**Why**: Every put encodes the `Composition` via `postcard::to_stdvec`.
With write-behind enabled, this encoding happens in the drainer task,
off the hot path. With write-behind disabled (the per-write-fsync
default), it's on the hot path. The drainer path is fine; the inline
path could amortize the encoding by batching multiple puts in one
encode pass.

**Expected lift**: only relevant when write-behind is disabled.
Marginal otherwise.

**Effort**: 0.5 day. **Risk**: Low.

### B9 — `tracing::debug!` macro expansion in hot paths ✅ **Picked up 2026-05-05** (S11 above)

Hot-path debug calls deleted entirely (entry / success / per-chunk markers / pre-Raft commit). `tracing::warn!` paths preserved — those signal real problems, not steady-state. Phase histograms cover the observability surface; for ad-hoc debugging set `RUST_LOG=kiseki_gateway=trace` and exercise the relevant non-hot endpoints.

### B10 — `compositions.log().cloned()` in the write path

**Where**: `InMemoryGateway::write` at the end of the create critical
section.

**Why**: Every PUT clones the `Option<Arc<dyn LogOps + Send + Sync>>`
out of the composition store. The Arc clone is cheap (atomic
reference bump) but it's still on every PUT. If the gateway cached
the log handle separately (set once at construction), the clone could
be eliminated.

**Expected lift**: <1 %.

**Effort**: <1 hour. **Risk**: Low.

---

## What to do with this backlog

1. **If you're already touching one of these code paths** for a
   different reason, look up the corresponding entry and fold the
   optimization in. The marginal cost is low when the surrounding
   code is already in flight; the marginal cost is high if you
   schedule it standalone.
2. **If a future perf measurement shows a specific hot frame** that
   matches one of these areas, the entry has the proposed fix and
   risk profile already worked out — saves the investigation step.
3. **B1 (DashMap) is the only entry that meaningfully improves the
   1.74× read/write ratio** toward the WekaFS-class 1× target. The
   rest are micro-optimizations that compound but don't change the
   shape of the curve. If WekaFS-parity becomes a hard target, B1 is
   the first ADR-grade follow-up.
4. **B5 (DEK cache)** is captured separately in the architect handoff
   for ADR-042 because it requires its own security-review-grade ADR.

When picking up an item, append a "**Picked up YYYY-MM-DD by ADR-XX**"
line so this doc tracks what's still open.
