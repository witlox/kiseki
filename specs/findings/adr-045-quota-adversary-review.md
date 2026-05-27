# Adversary review — ADR-045 §D6 (per-(namespace,tier) quota accounting + enforcement)

**Reviewer**: adversary role (inline, per the diamond workflow — Design
protocol: "Adversary reviews before implementation").
**Date**: 2026-05-27.
**Scope**: only §D6 (quota accounting + enforcement). §D1–D5 (placement
steering) already landed and is out of scope here.
**Verdict**: 3 High + 4 Medium findings. None block implementation if the
resolutions below are honored; they shape the data model and the
enforcement semantics.

## Findings

### F-1 (High) — The counter can't be derived from the chunk store
The chunk store is **namespace-agnostic** (chunks are content-addressed
and shared across namespaces via dedup; a `ChunkRecord` has `pool_name`
i.e. the tier, but no `namespace_id`). So §D6 option (a) — "rebuild
counters from a chunk-meta boot scan" — is **not implementable**: the
chunk meta cannot attribute bytes to a namespace.
**Resolution**: per-(namespace,tier) *logical* usage is derived from the
**composition layer**, not chunks. A composition belongs to a namespace
and carries its logical size. To get the *tier* split, record the placed
tier on the composition record (small additive field; the gateway knows
the resolved tier at write time — I-TQ2). Boot rebuild = group the
node's compositions by (namespace, tier) and sum sizes. New invariant
**I-TQ5**: per-(namespace,tier) usage is rebuilt from composition
records (namespace + size + placed-tier), not from the chunk store.

### F-2 (High) — Counters drift on the EC fragment path
A chunk's bytes are charged once (logical payload), but EC writes fan out
fragments to N nodes. If each node counts its local fragment against the
namespace quota, the namespace is charged ~1.5× (EC-4+2) of its logical
size on *every* node — wildly over-counting cluster-wide, and
inconsistently per node.
**Resolution**: the quota is **logical**, charged **once per chunk on the
leader/origin** (the node that runs `write_chunk` for the gateway), NOT
per fragment on receivers. Receivers (`write_fragment_in_pool`) do **not**
touch quota counters. The durability multiplier (ADR-045 §D6) is applied
only when comparing logical usage against *raw* tier capacity for the
ENOSPC decision, not stored in the counter. I-TQ3 already says quota is
logical; this pins *where* it's charged. New **I-TQ6**: exactly one node
(the gateway's origin) charges a chunk's logical bytes to the
(namespace,tier) counter.

### F-3 (High) — Refcount/dedup makes "bytes used" ambiguous
With content dedup (ADR-044), two namespaces writing identical content
share one physical chunk (refcount=2). Charging both namespaces the full
logical size double-counts physical capacity; charging neither (or
splitting) makes the quota unpredictable for the operator.
**Resolution**: quota accounts **logical bytes the namespace addresses**
(what the client PUT), not physical post-dedup bytes — matches how S3/GCS
bill. A dedup hit still charges the namespace's logical bytes (the
namespace "uses" that much logically). Physical savings show in the
*capacity* view (dedup ratio, already shipped), not the quota view. The
two are different questions and must not be conflated. Document on the
`capacity --namespace` output.

### F-4 (Medium) — Soft-quota overshoot is unbounded under burst
ADR-045 accepts soft quota (overshoot reconciled later). But without a
bound, a burst of concurrent large writes can overshoot a tier by an
arbitrary amount before the counter catches up.
**Resolution**: the check + increment happen under the same lock that
guards the counter map (a short critical section, not held across I/O),
so overshoot is bounded by the number of *concurrent* writes × max chunk
size, not unbounded. The check is "would this write cross the cap?" → if
so, spill/reject *before* the device write. Documented as the overshoot
bound. (Hard transactional quota is still rejected — too costly on the
write path.)

### F-5 (Medium) — `tier_policy` replication race vs first write
A client can PUT to a namespace on the leader the instant after
`namespace-create` returns, before the `NamespaceCreate` delta has
hydrated on a follower that the write later reads from. The leader has
the policy (added locally first); a follower might not yet.
**Resolution**: placement (which tier) is decided on the **origin/leader**
that has the policy (it added the namespace locally before emitting the
delta — existing pattern). Followers only *receive fragments* (no quota
decision). So the race doesn't affect placement or accounting. The only
follower-visible effect is a brief window where a cross-node *read* of
the namespace's policy (for `capacity --namespace`) is stale — cosmetic,
converges on the next hydrator poll.

### F-6 (Medium) — Quota on delete must not underflow
Decrementing the counter on delete/refcount-drop can underflow if a
delete is replayed (hydrator at-least-once) or a chunk was never charged
(pre-existing data from before the feature).
**Resolution**: saturating subtraction; counters are advisory/derived and
reconciled by the next boot scan, so a transient underflow-to-zero is
self-healing. Never let the counter wrap.

### F-7 (Medium) — Per-tier cap with empty/missing tier in the policy
A write hinted to a tier the namespace's policy doesn't list (or a
namespace with no policy) has no cap to check.
**Resolution**: no policy ⇒ no quota enforcement (unbounded, current
behavior). A hint to an unlisted tier is treated as "no cap for that
tier" but still placed (the §D1 fastest-fit/spill governs placement).
Quota only constrains tiers the operator explicitly bounded.

### F-8 (High) — Per-namespace usage is cluster-wide, not per-node
A namespace's writes go to whichever node the client/gateway hits, so a
single node only sees the bytes *it* wrote. The quota is a **cluster-wide**
sum, but enforcement happens locally at write time on one node, which only
has its own counts + (stale) cached peer snapshots.
**Resolution**: reuse the capacity-aggregation pattern already shipped
(GH #115): each node exports per-(namespace,tier) usage; the
`MetricsAggregator` sums across nodes for the cluster view + the
`capacity --namespace` report. Enforcement is **soft + cluster-stale**:
the writing node checks `(its own count + last-aggregated peer counts) +
this write` against the cap. Overshoot is bounded by per-node concurrency
× scrape interval, reconciled each aggregation. This is consistent with
the soft-quota decision (§D6) and with how object stores enforce quotas
(eventually, not transactionally). A *hard* cluster-wide quota would need
a consensus round per write — explicitly rejected.

## Net data-model + enforcement shape (for the implementer)

- Counter: `Mutex<HashMap<(NamespaceId, tier_string), u64>>` (logical
  bytes), seeded in the `chunk_map` build pass (I-TQ5), charged once on
  the origin (I-TQ6), saturating on delete (F-6).
- Enforcement at the gateway, before the chunk write: resolve tier
  (§D5) → if the namespace's policy bounds that tier and
  `used + logical_bytes > quota` → **spill** to the next tier in the
  policy → if none has room → `InsufficientStorage` (507). Under the
  counter lock (F-4).
- `capacity --namespace <id>`: per-tier used/quota/% (logical), with a
  note that physical dedup savings live in the cluster capacity view
  (F-3).

These resolutions are additive to ADR-045 (no design reversal). Fold
I-TQ5/I-TQ6 into the invariants list when the implementation lands.
