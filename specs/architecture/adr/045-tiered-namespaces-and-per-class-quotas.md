# ADR-045: Tiered Namespaces, Per-Class Quotas, and Placement Steering

**Status**: Proposed (awaiting adversary review before the quota-accounting
implementation lands; the placement-steering substrate in §D1–D5 is
lower-risk and implemented incrementally).
**Date**: 2026-05-27.
**Deciders**: Architect role; implementer after adversary sign-off on §D6.
**Context**: ADR-005 (EC + durability), ADR-022 rev-4 (fjall meta),
ADR-024 (device management, classes, capacity thresholds, tiering),
ADR-025 (storage admin API — quotas), ADR-029 (raw block allocator),
ADR-030 (small-file placement + metadata budget), GH #115 (the capacity
substrate this builds on).

## Problem

ADR-024 establishes heterogeneous JBOD by device class (NVMe / SSD / HDD)
so data can be placed cost/performance-optimally — hot on fast NVMe, cold
on cheap HDD. GH #115 delivered the *substrate*: each node opens its full
device pool (`KISEKI_RAW_DEVICES`), placement is class-aware
(fastest-tier-first with spill), and capacity is reported per class
cluster-wide. What is still missing is the *steering*: nothing tells the
system which class a given namespace's data should use, or how much of
each class it may consume.

The motivating ask: **create a namespace with a controlled per-class
allocation — e.g. 10 TB on NVMe + 100 TB on HDD — across the cluster.**
Today every write falls through to `write_chunk(env, "default")` and the
EC fan-out targets all peers, so a namespace fills the fastest tier first
and overflows uncontrolled. There is no per-namespace, per-class quota.

This is a *tiered namespace*: a namespace bound to multiple device-class
tiers, each with a quota, with a policy deciding which object lands on
which tier.

## Decision

### D1. `StorageTier` is the placement unit

Three cost/performance tiers, derived from the probed medium (GH #115):

| Tier | Media | Use |
|------|-------|-----|
| `Fast` | NVMe SSD | hot / latency-sensitive |
| `Bulk` | SATA/SAS SSD (+ virtual/unknown) | warm |
| `Cold` | rotational HDD | cold / archival |

`StorageTier::of(medium)` is the single mapping; `DevicePool` already
groups its heterogeneous members by tier and places fastest-first.

### D2. Class pools are auto-derived, not admin-created

ADR-025's `CreatePool` admin API stays Proposed and is **not required**
for tiering. The three tiers above *are* the pools, derived from each
node's device media at boot. The cluster-wide "NVMe pool" is the union of
every node's `Fast`-tier devices. This removes a configuration step:
operators provision devices; the tiers exist. (Named pools with custom
device-class membership remain a future ADR-025 refinement; tiering does
not block on it.)

### D3. Namespace tier-affinity is declared at create time

The create command that already distributes shards across the cluster
(`topology namespace-create <ns-id> --tenant <id> --shards N`) gains the
resource-steering dimension:

```
kiseki-admin topology namespace-create <ns-id> --tenant <id> --shards N \
    --tier fast=10T \
    --tier cold=100T
```

- `--tier <class>=<quota>` is repeatable; order is the **spill order**
  (first tier preferred, overflow to the next).
- `<quota>` is **logical/usable** bytes (`10T`, `500G`, or `0`/unset =
  unbounded). The accounting (§D6) applies the tier's durability
  multiplier to get raw bytes.
- A bare `--class <fast|bulk|cold>` is sugar for a single unbounded tier.
- **Default** (no `--tier`): one unbounded `Fast`-preferred policy — i.e.
  exactly today's behavior (fastest-fit, spill). Backward compatible.

The namespace topology record carries
`tier_policy: Vec<TierQuota { tier: StorageTier, quota_bytes: Option<u64> }>`.
The record is replicated through the same control-plane path that
distributes the shards (`emit_namespace_create`), so every node agrees on
the policy (determinism — see I-TQ1).

### D4. Placement steering: tier = local device choice, EC = cross-node durability

The key decoupling: **a tier selects which device class holds a fragment
on each node; EC/replication selects which nodes hold fragments.** They
are orthogonal when every storage node carries every tier (the common
deployment).

Write path:
1. Gateway resolves the effective tier for the object (§D5) → a
   `StorageTier` hint.
2. The hint rides the existing `pool` argument of `write_chunk` (the
   `pool` string maps to a tier) down to the chunk store.
3. The chunk store calls `DeviceBackend::alloc_in_tier(size, hint)`
   (default impl ignores the hint → `alloc`; `DevicePool` honors it:
   place on the requested tier's members, spill cost-ordered if full).
4. The EC fan-out is **unchanged** — fragments still spread across N nodes
   for durability; on each node the local `DevicePool` lands the fragment
   on the hinted tier.

**Non-uniform fleets** (e.g. dedicated NVMe nodes with no HDD): the local
tier may be absent on some nodes, so a `Fast`-tier write can't be honored
everywhere. Class-aware *node* selection (fan-out restricted to nodes
that carry the tier) is **deferred** — it changes EC durability placement
and needs its own analysis. Until then, a tier absent on a node spills to
that node's nearest available tier (logged). The common
"every-node-mixed-media" case is fully honored. See Open Questions.

### D5. Tiering decision — which object goes to which tier

- **MVP — explicit.** Per-request hint (S3 `x-amz-storage-class`:
  `STANDARD`→Fast, `STANDARD_IA`/`GLACIER`→Bulk/Cold; native API field)
  overrides; absent → the namespace policy's first tier.
- **Future — reactive auto-tiering** (ADR-024 §"Reactive tiering"):
  promote hot / demote cold by access frequency, bounded by the per-tier
  quota. Out of scope here; the quota envelope (§D6) is what bounds it.

### D6. Per-(namespace, tier) quota accounting + enforcement

**Accounting.** A counter per `(namespace_id, tier)` tracks logical bytes
stored. On write: `effective = payload_bytes × durability_multiplier(tier
pool)` (EC-4+2 ≈ 1.5×, R-3 = 3×) — the quota is logical but the *check*
is against raw capacity, so the multiplier is applied consistently with
the tier's `DurabilityStrategy`. On delete/refcount-drop: decrement.

**Enforcement** (ADR-024 thresholds, per tier):
- Under quota → place on the tier.
- Tier at quota → **spill to the next tier in policy order** (§D3); if no
  tier in the policy has room → `InsufficientStorage` (S3 507 / native
  `resource_exhausted`, the GH #115 clean-ENOSPC path).
- Pool-level ADR-024 thresholds (Warning/Critical/ReadOnly/Full) still
  apply underneath the namespace quota.

**Crash safety + replication (the part needing adversary review).** The
counters are derived state. Options under review: (a) rebuild from the
chunk meta on boot (like the chunk_map hydration) + maintain in-memory
deltas — simple, no new durable state, bounded by an O(chunks) boot scan;
(b) persist counters alongside the namespace record. (a) is preferred
(no new crash-consistency surface), accepting that a quota check races
slightly under concurrent writes (soft quota — overshoot bounded by
in-flight writes, reconciled on the next scan). This matches how cloud
object stores treat quotas (eventually-enforced, not hard transactional).

### D7. Capacity reporting per (namespace, tier)

`kiseki-admin capacity` already shows cluster + per-class + dedup (GH
#115). Tiered namespaces add `kiseki-admin capacity --namespace <id>`:
per-tier used / quota / % for that namespace, mirroring the cluster view.

## Invariants (Proposed)

- **I-TQ1**: A namespace's `tier_policy` is identical on every node
  (replicated via the control-plane namespace-create path). Placement is
  deterministic given the policy + the object's tier hint.
- **I-TQ2**: A write is charged to exactly one `(namespace, tier)` counter
  — the tier it actually landed on (after any spill), not the tier
  requested. Spill is observable (`kiseki_namespace_tier_spill_total`).
- **I-TQ3**: Quota is logical; the enforced figure is
  `logical × durability_multiplier`. A namespace's raw consumption never
  silently exceeds the sum of its tier quotas × multipliers without a
  `Warning`/`Critical` capacity signal.
- **I-TQ4**: Deleting all of a namespace's objects returns its per-tier
  counters to zero (no accounting leak; reconciled by the boot scan).

## Consequences

### Positive
- "10 TB NVMe + 100 TB HDD in one namespace" is expressible and enforced.
- Cost/performance placement is operator-controllable at the create seam
  that already exists, without a separate pool-config step.
- Builds entirely on the GH #115 substrate (tier-aware pool, per-class
  capacity); no new device-layer concepts.

### Negative
- Soft quota (D6 option a): brief overshoot under concurrent writes,
  reconciled on scan. Hard transactional quota is rejected as too costly
  on the write path (matches S3/GCS semantics).
- Non-uniform fleets aren't fully honored until class-aware node selection
  lands (Open Questions); mixed-media nodes are.
- Boot scan cost is O(chunks) for counter rebuild — same order as the
  existing chunk_map hydration, acceptable at the in-RAM chunk-map scale.

### Neutral
- Default namespace behavior is unchanged (single unbounded Fast tier).

## Alternatives considered

- **Admin-created named pools (full ADR-025) as the binding unit** —
  more flexible (custom device-class membership) but adds a config step
  and a pool-membership subsystem. Rejected for the MVP; tiers auto-
  derived from media cover the motivating case. ADR-025 can layer named
  pools on top later.
- **Hard transactional per-write quota** — correct but serializes the
  write path through a quota txn. Rejected (D6); soft quota + scan
  reconciliation matches object-store norms.
- **Class-aware EC node selection now** — needed for non-uniform fleets
  but changes durability placement; deferred to its own ADR so EC
  correctness gets dedicated analysis.

## Open questions / future work

1. **Class-aware cross-node placement** for non-uniform fleets (dedicated
   tier nodes). Restricts EC fan-out to nodes carrying the tier; interacts
   with EC durability (fewer eligible nodes ⇒ fewer distinct failure
   domains). Sibling ADR.
2. **Reactive auto-tiering** (ADR-024) — promote/demote by access
   frequency within the quota envelope.
3. **Tenant-level quotas** (ADR-025) — the per-(namespace,tier) counters
   here are the building block; tenant = sum over its namespaces.
4. **Durability per tier** — `--tier fast=10T:ec4+2 cold=100T:r3`?
   `DurabilityStrategy` is already per-pool; exposing it per tier on the
   create command is a small follow-up.

## Adversary review

Required before §D6 (quota accounting/enforcement) implementation lands.
Focus: crash-safety of the derived counters (boot-scan correctness under
partial writes / orphan extents), the soft-quota overshoot bound under
concurrent + cross-node writes, replication of `tier_policy` vs. the
namespace-create race, and the durability-multiplier accounting against
mixed EC/replication pools. §D1–D5 (placement steering, no durable quota
state) may land ahead of review as it only routes existing writes.
