# ADR-049: Cluster-side device inventory + per-tier metadata placement

**Status**: Proposed
**Date**: 2026-06-02
**Deciders**: Architect role; implementer + adversary review before code
**Context**: #129 perf-unblock surfaced that small-object store on redb is
the bottleneck. The fjall swap (ADR-022 rev-5) needs to know **where**
to put the fjall directory. There is no current mechanism to route a
fjall store to a specific device — every fjall store today is hardcoded
under `KISEKI_DATA_DIR`, regardless of available NVMe / SSD / HDD.
**Related ADRs**: ADR-022 (fjall keyspace pattern), ADR-024 (device
management + capacity), ADR-029 (raw block allocator), ADR-030 (small-
file placement), ADR-040 (persistent metadata stores), ADR-045 (tiered
namespaces).

## Revision history

- **rev 5** (2026-06-02, this revision): fast-tier mount aggregation
  is an **OS-layer LVM concern**, NOT a kiseki concern. The setup
  script provisions each catalog-resolved fast-tier mount
  (`fast-small`, `fast-meta`) as an ext4 filesystem on a striped LV
  spanning N physical NVMe (default N=1 — a degenerate single-disk
  stripe LV — so capacity expansion has the same shape regardless of
  starting disk count). Adding capacity is the standard online LVM
  dance — `pvcreate` → `vgextend` → `lvextend` → `resize2fs` — with
  zero kiseki involvement: the mount path stays the same, the
  pointer file stays the same, fjall picks up the bigger
  `statvfs()` result on next compaction. **I-CP-Move is rescoped**
  to "mount-path change" (operator policy edits / disk replacement);
  it explicitly does NOT fire for capacity expansion. The §D8
  migrate CLI is correspondingly rescoped: it moves a tier between
  mount paths (rare), not "add capacity" (common, handled at the
  OS layer). New §D8.2 documents the LVM layout convention; §D11.1
  notes the boundary explicitly. No code changes — resolver +
  pointer file + capacity formula all consume `statvfs()` totals
  which already reflect LVM-aggregated capacity. Chunk-pool axis
  unchanged (raw block, no FS, no LVM — `KISEKI_RAW_DEVICES`).
- **rev 1** (2026-06-02): initial draft.
- **rev 2** (2026-06-02): cluster-aggregate capacity formula
  (§D4.5), per-node distribution, capacity admin surface.
- **rev 4** (2026-06-02, this revision): MUST-FIX items from the
  re-diamond (rev 3 verification round). Four spec corrections:
  - **N-12 / architect**: `PER_FILE_METADATA_FOOTPRINT_BYTES`
    moves from `kiseki-server` (binary-only crate, not importable)
    to `kiseki-common::metadata`. Phase 1 of implementation
    relocates the constant + updates the two existing call sites.
  - **N-5 / architect**: `await_catalog_ready` quiescence-vs-timeout
    interaction. `last_change_ms` split into `policy_change_ms`
    (the quiescence clock — only mutated by `SetPlacementPolicy` /
    `SetWorkloadParams`) and `inventory_change_ms` (D10
    observability only — NOT consulted by quiescence). Defaults
    rebalanced: `KISEKI_CATALOG_BOOT_TIMEOUT_MS=90 s`,
    `KISEKI_CATALOG_QUIESCENCE_MS=30 s` (was 30 / 120).
  - **Auditor STILL-FAIL**: worked-example arithmetic recomputed
    bit-exact. `projected_metadata = 287.987 GiB` (was 309.1 GiB
    — residual GB-vs-GiB drift the rev-3 audit missed because
    the per-node-sum identity hid it). Cascades:
    `metadata_budget` 463.6 → **431.98 GiB**; `small_file_budget`
    (homogeneous) 6448.4 → **6480.02 GiB**; per-node metadata
    77.3 → **71.997 GiB**; per-node small 1074.7 → **1080.00 GiB**;
    heterogeneous node-4 line items 12.04/137.94 → **11.22/138.79
    GiB**; node-1 line items 90.31/1034.74 → **84.16/1040.84 GiB**.
    Per-node-sum identity (200.00 / 1500.00 GiB) holds either way.
  - **Auditor Q10**: per-node-floor redistribution algorithm now
    spec'd with explicit pseudocode (deterministic donor
    selection, contribution formula, per-tier ordering). I-DI10
    holds across the algorithm.
  Documents the 12 phase-3 acceptance criteria (adversary
  N-1..N-12) + 5 architect SHOULD-FIX items as numbered phase
  commitments in open questions Q22-Q38.
- **rev 3** (2026-06-02): incorporates gate-1
  diamond findings (adversary F-1..F-17 + architect coherence
  audit + auditor numeric audit). Material changes:
  - **F-1 cold-boot deadlock** (CRITICAL): RaftLog tier is
    bootstrap-only, env-var-driven (`KISEKI_RAFT_LOG_DIR`); never
    resolver-routed. The control-plane Raft fjall therefore opens
    BEFORE the catalog exists, breaking the circular dependency.
    See §D2.5.
  - **F-3 I-CP1 path-move loss** (CRITICAL): new invariant
    **I-CP-Move**, path-version pointer file in `DataDir`, fjall
    keyspace refuses to open at a new path when a non-empty
    keyspace exists at the prior resolved path. See §D8.1.
  - **F-4 apply-time I-DI8 enforcement** (CRITICAL):
    `SetPlacementPolicy` and `SetWorkloadParams` apply rejects
    in Strict mode when any node would violate I-DI8 after
    re-resolution against current inventories. New invariant
    **I-DI9 (policy apply gate)**.
  - **Architect: §D2 enum rewrite**: the catalog lives on
    `ControlCommand` (control-plane state machine,
    `serde_json` encoding) — NOT `LogCommand` (per-shard,
    postcard). Whole §D2 rewritten against the real types.
  - **Auditor: formula edge cases**: `F_total == 0` explicit
    re-base on `S_total`; worked-example shares corrected from
    19.5% → 19.04%; decimal/binary drift fixed (921.6 GiB, not
    900 GiB); canonical summation order pinned for I-DI7
    determinism (new **I-DI10**).
  - **WorkloadParams** constants composed from
    `system_disk::PER_FILE_METADATA_FOOTPRINT_BYTES` × R_metadata
    instead of duplicating the 1.5 KiB figure.
  - **Cross-ADR contradictions resolved**: §D11 explicitly
    enumerates the boundary with ADR-024 (DeviceClass refinement
    of MediaType), ADR-030 (ADR-049 owns small_file_budget_bytes;
    ADR-030 §3 I-L9 consumes the resolver), ADR-045 (Inline
    durability namespaces consume RaftLog budget per
    `inline_payload_factor`; not SmallObject).
  - **Per-node floor (Q10) resolved**: BestEffort floors at
    `per_tier_min_viable_bytes` (Phase 3 measurement target,
    initial estimate 256 MiB) with redistribution from
    largest-share nodes; Strict rejects.
  - **Admin verb disambiguation**: "tier" overloaded — ADR-049
    uses `FjallStoreTier` internally but the admin surface
    verbs become `placement-policy set-store-prefs` and
    `capacity-policy set-store` to disambiguate against ADR-024
    size-band-tier and ADR-045 namespace-quota-tier.
  - **Missing interfaces added**: `DeviceResolver` trait,
    `DeviceCatalogRead` trait, `await_catalog_ready`,
    `InventoryReporter`, pure `compute_cluster_budgets` +
    `distribute_to_node`, `resolve_all()`. See §D5.5.
  - **fast_headroom_pct default bumped 10 → 25** (F-8): LSM
    leveled-compaction worst-case is 30-50% during merge;
    25% balances headroom against SmallObject capacity. Phase
    3 measurement target: existing fjall stores' actual
    compaction overhead.
  - **BDD scenario IDs pre-allocated**: DI-1..DI-5 listed in
    §D12 phase 6.
  - New invariants I-CP-Move, I-DI9, I-DI10. New §D2.5
    (RaftLog bootstrap), §D5.5 (interfaces), §D8.1 (path-move
    semantics).

## Context

### What we have today

- `KISEKI_RAW_DEVICES` lists raw block devices for the chunk store
  (`RawBlockDevice`, O_DIRECT, EC fragments). Operator manages this
  list; nodes consume it locally.
- `KISEKI_DATA_DIR` is a single directory path. Every fjall store
  (composition meta, intent store, raft log, chunk meta, soon small
  store) is hardcoded under it.
- `crates/kiseki-server/src/system_disk.rs::detect_media_type` exists
  and reads `/sys/block/<dev>/queue/rotational` — returns
  `Nvme | Ssd | Hdd | Unknown` for a given path.
- `crates/kiseki-chunk/src/pool.rs::AffinityPool` carries a
  `device_class: DeviceClass` field. `select_pool_for_write` does
  band-aware placement for chunk pools.
- No per-node device inventory is published to a cluster-shared
  catalog. No fjall store consults `AffinityPool` for its location.

### What we don't have

- Per-node device inventory: a node reporting "I have `/mnt/nvme0`
  (NVMe), `/mnt/sata0` (SSD), `/` (HDD)" to anyone except its own
  metrics.
- Per-fjall-tier placement policy: "small-object store goes on
  NVMe-class devices; intent store goes on NVMe-class devices; raft
  log goes on NVMe-class devices; composition meta goes on
  NVMe-or-SSD-class".
- A per-node resolver that consults the policy + local inventory and
  picks a path for each fjall store at boot.
- Cluster-side admin to set / inspect the policy.

### Why now

#129 wants multi-node inline small-file writes. The redb-backed
`SmallObjectStore` is a serialized Mutex + per-write fsync — the same
shape ADR-022 rev-2/3/4 moved everything else off. The fjall swap
needs an answer to **where to put the fjall directory** that doesn't
collapse back onto root disk on nodes with NVMe available.

Without inventory + placement, every fjall swap leaks the same
question — and the answer is always "depends on operator mount
strategy + filesystem layout luck". That's not a cluster property,
it's per-deploy improvisation. Heterogeneous hardware breaks it.

## Decision

Introduce **per-node device inventory** (each node publishes its
filesystem mount points + detected media class to a cluster catalog)
and **per-tier placement policy** (admin-defined rules saying "this
tier prefers this device class"). Every fjall store consults a
per-node resolver at boot that joins local inventory + cluster policy
and picks a path.

The model has three pieces:

1. **Per-node device inventory** — what's available on each node.
2. **Per-tier placement policy** — cluster-wide rule for each fjall
   store class.
3. **Per-node resolver** — joins (1) and (2) at boot to pick paths.

### D1. Per-node device inventory

A `NodeDeviceInventory` is a list of `DeviceEntry` records that the
node has discovered (auto) or been told about (operator):

```rust
struct DeviceEntry {
    /// Filesystem mount path, e.g. "/mnt/nvme0".
    mount_path: PathBuf,
    /// Detected media class via system_disk::detect_media_type.
    media_class: MediaType,            // Nvme | Ssd | Hdd | Unknown
    /// Filesystem total bytes (df-style).
    total_bytes: u64,
    /// Free bytes at last refresh.
    free_bytes: u64,
    /// Operator-supplied tag, e.g. "nvme-fast" or "boot-disk".
    /// Optional — enables policy that targets a specific tag rather
    /// than a class.
    tag: Option<String>,
    /// Whether the node has exclusive ownership of this mount
    /// (true) or shares it with other services (false). Affects
    /// fsync latency assumptions.
    exclusive: bool,
}

struct NodeDeviceInventory {
    node_id: NodeId,
    devices: Vec<DeviceEntry>,
    /// Wall-clock at last refresh.
    refreshed_ms: u64,
}
```

#### Discovery

At boot, each node:

1. Reads `/proc/mounts` (Linux). For each mount whose fstype is
   real-storage (ext4, xfs, btrfs, zfs, tmpfs excluded), call
   `detect_media_type` on the mount path.
2. Filters to mounts the operator has flagged via
   `KISEKI_DEVICE_TAGS` (a CSV of `path=tag` pairs, e.g.
   `/mnt/nvme0=nvme-fast,/mnt/sata0=ssd-tier`). Untagged mounts
   are still included with `tag: None` so auto-policies can use
   them, but tagged-target policies skip them.
3. Always includes `KISEKI_DATA_DIR`'s mount as a fallback
   `DeviceEntry` (tag `data-dir-default`). Boot-disk mounts are
   marked HDD-class via detection or `Unknown` if probing fails.
4. Publishes the inventory via the boot heartbeat (D3 below).

Refresh: every 60 s the node re-runs steps 1-3 and re-publishes if
anything changed (free bytes drift > 1 %, new mount discovered,
removed mount). Hardware-change is operator-driven for now (no
`udev` listener in v1).

### D2. Cluster catalog (lives on the control-plane state machine)

Inventories live in the **control-plane Raft group** (existing
ADR-033/§4 multiplexed listener, ADR-041 multiplexed transport).
The concrete types are in `crates/kiseki-server/src/cluster_control/`:

- Enum: `ControlCommand` at `commands.rs:20-87` — variants today
  are `CreateNamespace`, `RecordSplit`, `RecordMerge`,
  `RetireShard`. **NOT** the per-shard `LogCommand` enum.
- State machine: `ControlStateMachine` at `state_machine.rs:285-438`,
  `apply_command()` at `state_machine.rs:139`.
- Snapshot: `ControlSnapshot` at `state_machine.rs:113` — encoding
  is **`serde_json`** (see `state_machine.rs:452`). New optional
  fields can be added without breaking pre-upgrade snapshots
  (serde_json tolerates unknown fields and missing-but-defaulted).
- Group id: `CONTROL_RAFT_GROUP_ID` at `mod.rs:49`.

State the catalog adds to `ControlStateMachine`'s inner:

```rust
struct ClusterDeviceCatalog {
    /// One entry per node. Keyed for O(log N) upsert; iteration
    /// in `NodeId` ascending order is canonical (see I-DI10).
    inventories: BTreeMap<NodeId, NodeDeviceInventory>,
    policy: PlacementPolicy,
    workload: WorkloadParams,
    policy_revision: u64,                  // monotone, bumped per Set*
    /// Wall-clock at last **policy or workload** mutation. Bumped
    /// by `SetPlacementPolicy` and `SetWorkloadParams` apply ONLY
    /// — NOT by `UpsertNodeInventory`. This is the
    /// `await_catalog_ready` quiescence clock (§D5.5); inventory
    /// upserts happen every 60 s per node and would otherwise
    /// reset quiescence on every busy-cluster apply, preventing
    /// boot from ever proceeding (rev 4 fix for re-diamond N-5).
    policy_change_ms: u64,
    /// Separate wall-clock for inventory drift, used by D10
    /// observability gauges but NOT by `await_catalog_ready`.
    inventory_change_ms: u64,
}
```

Why the control-plane group (not per-shard Raft, not gossip):
- **Per-shard** is wrong: device inventory is node-scoped, not
  shard-scoped. Multiple shards on one node share the same devices.
- **Gossip** loses idempotency: a node restart that re-publishes
  inventory needs the catalog to converge deterministically.
- **Control plane** is the right scope: cluster-wide policy lives
  here next to namespace topology and shard membership.

Mutations: three new `ControlCommand` variants. JSON encoding means
discriminant order is **not** load-bearing; we still append for
review-diff cleanliness:

```rust
// crates/kiseki-server/src/cluster_control/commands.rs
pub enum ControlCommand {
    CreateNamespace { … },
    RecordSplit { … },
    RecordMerge { … },
    RetireShard { … },
    // NEW (ADR-049 rev 3):
    UpsertNodeInventory { node_id: NodeId, inventory: NodeDeviceInventory },
    SetPlacementPolicy { policy: PlacementPolicy },
    SetWorkloadParams  { params: WorkloadParams },
}
```

`SetWorkloadParams` is a separate variant from `SetPlacementPolicy`
because operators tune them at different cadence (workload =
quarterly capacity planning; policy = rare, after hardware change).
Each apply runs the §D9 **I-DI9 (policy apply gate)** — re-resolves
budgets against current inventories and refuses to commit when any
node would violate I-DI8 in Strict mode.

Reads are local (every node has the catalog in
`ControlStateMachine`'s inner state). No new RPC for reads — admin
CLI hits the existing `/admin/topology/...` HTTP surface.

### D2.5. RaftLog tier is bootstrap-only (F-1 cold-boot fix)

The control-plane Raft state machine itself uses fjall (per ADR-040
+ ADR-049 RaftLog tier). Routing the control-plane RaftLog through
the catalog resolver would deadlock: the resolver needs the policy,
the policy lives in the catalog, the catalog lives in the
control-plane state machine, the state machine can't open until its
fjall path is known. **Cold boot — including the cluster bootstrap
seed node — fails on every brand-new deploy.**

Resolution: **the RaftLog tier is bootstrap-only.** Its path comes
from `KISEKI_RAFT_LOG_DIR` (env var, falls back to
`KISEKI_DATA_DIR/raft`). It is **NEVER** resolver-routed and **NEVER**
appears in `PlacementPolicy.tiers`. Listed in §D4's default table
for cross-reference but with capacity = `BootstrapOnly`.

Consequence:
- The control-plane Raft fjall opens at the bootstrap path BEFORE
  the catalog exists. I-DI3 is amended below to exempt RaftLog.
- The other four tiers (SmallObject, IntentStore, CompositionMeta,
  ChunkMeta) open AFTER the catalog policy is read. I-DI3 still
  applies to them.
- An operator wanting RaftLog on a fast device sets
  `KISEKI_RAFT_LOG_DIR=/mnt/nvme0/kiseki/raft-log` at node startup.
  Per-node, not cluster-wide.

`KISEKI_DATA_DIR` itself remains a single env var; `KISEKI_RAFT_LOG_DIR`
is the only new mandatory bootstrap path knob. The infra/gcp boot
script (phase 7) wires both atomically.

The amended I-DI3 reads: "runtime.rs MUST NOT open any **catalog-
resolved** fjall store before the catalog policy is read.
Bootstrap-only stores (RaftLog) open before the catalog at
their env-var paths."

### D3. Inventory publish path

A node publishes its inventory two ways:

1. **Boot publish**: kiseki-server, after Raft membership is up but
   before any fjall store is opened, reads its local inventory,
   submits `UpsertNodeInventory` to the control-plane group via
   the leader-forward path (ADR-042 §4), and **waits for the apply
   to commit** before continuing boot. This ensures the catalog
   reflects this node before the resolver runs.
2. **Periodic refresh**: every 60 s after boot, re-run inventory.
   If changed, submit `UpsertNodeInventory`. No boot wait.

Bootstrap edge case: when the cluster is brand-new (no control-plane
leader yet), the seed node uses its **local** inventory directly
without going through Raft. The first `UpsertNodeInventory` lands
once membership initializes. Other nodes follow normally.

### D4. Placement policy

A policy assigns each fjall store tier to a device-class preference
list:

```rust
enum FjallStoreTier {
    SmallObject,            // crates/kiseki-chunk small-store
    IntentStore,            // ADR-047 intent
    RaftLog,                // openraft fjall_log_store
    CompositionMeta,        // ADR-040 composition store
    ChunkMeta,              // ADR-022 chunk meta
}

struct TierPolicy {
    tier: FjallStoreTier,
    /// Ordered preference list. Resolver walks left-to-right.
    /// `Tag(s)` matches DeviceEntry.tag == Some(s).
    /// `Class(c)` matches DeviceEntry.media_class == c.
    /// `DataDir` matches the fallback boot-disk entry.
    /// Strict mode: refuses to resolve to any entry past the first
    ///   matching entry (operator error if it's not present).
    /// Best-effort: walks down the list until it finds any match.
    preferences: Vec<DeviceMatcher>,
    mode: PolicyMode,                  // Strict | BestEffort
    /// Cluster-wide budget the resolver distributes to this node
    /// proportional to its local fast-tier share. See §D4.5 for
    /// the formula. Per-node `ResolvedTierBudget.budget_bytes`
    /// comes from this field.
    capacity: TierCapacity,
}

enum DeviceMatcher {
    Tag(String),
    Class(MediaType),
    DataDir,
}

enum PolicyMode { Strict, BestEffort }

enum TierCapacity {
    /// Default formula: auto-derive cluster budget from
    /// `WorkloadParams` + per-tier clamps. Distributes per-node
    /// proportionally to local fast-tier capacity.
    Auto {
        /// Target fraction of cluster fast tier (`F_total`).
        /// Used directly for tiers that don't tie to chunk capacity
        /// (e.g. SmallObject pct=80, Headroom pct=10). For Metadata
        /// the formula in §D4.5 derives the target from
        /// `WorkloadParams`; this field then acts as the clamp
        /// ceiling (default 30 — see `metadata_ceiling_pct_of_fast`).
        target_pct: u8,
        /// Cluster-wide minimum (sum over nodes). Common floor:
        /// `10 GiB × N_nodes` for Metadata so a brand-new cluster
        /// with no chunks yet still has working metadata budget.
        floor_bytes: u64,
        /// Cluster-wide maximum. Optional. For Metadata: cap at
        /// `metadata_ceiling_pct_of_fast × F_total` so a high
        /// projection doesn't starve SmallObject.
        ceiling_bytes: Option<u64>,
    },
    /// Explicit absolute cluster-wide budget. Overrides Auto.
    /// Distributed per-node proportionally to local fast-tier share.
    Absolute { cluster_bytes: u64 },
    /// "Consume whatever's left after other tiers" — for Chunks. The
    /// resolver computes `node.chunk_budget = node.slow_capacity +
    /// (node.fast_capacity - Σ other_tier_budgets_on_this_node)`.
    Remainder,
}

struct PlacementPolicy {
    tiers: Vec<TierPolicy>,
    /// Inputs to the §D4.5 capacity formula. Operator-overridable
    /// via `kiseki-admin topology workload set`. Default values
    /// listed inline below.
    workload: WorkloadParams,
}

struct WorkloadParams {
    /// Average file size assumption. Default 256 KiB — sweeps
    /// between HPC (>1 MiB → tune down) and small-file workloads
    /// (<64 KiB → tune up). Drives
    /// `projected_files = S_total / avg_file_bytes`.
    avg_file_bytes: u64,
    /// Metadata replication factor. Default 3 (Replication-3).
    /// Composes `metadata_per_file_bytes` with the per-replica
    /// footprint from `kiseki_server::system_disk`.
    metadata_replication: u8,
    /// Plan for N× metadata growth past projection. Default 1.5.
    /// Bigger value → reserves more fast tier for future files.
    growth_headroom: f32,
    /// Fraction of `F_total` always reserved for LSM compaction
    /// overhead. Default **25** (rev 3, was 10). Leveled compaction
    /// temporarily holds two copies of the level being merged —
    /// real worst-case is 30-50% for write-heavy workloads. 25%
    /// is a measured-pending compromise; phase 3 instruments
    /// existing fjall stores and re-tunes. Without it, fsync
    /// stalls under sustained write pressure when memtable
    /// flushes hit level merge.
    fast_headroom_pct: u8,
    /// Maximum fraction of `F_total` Metadata can take. Default
    /// 30. Prevents projection over-estimate from starving
    /// SmallObject. Acts as Auto-mode ceiling for Metadata when
    /// the formula's natural output exceeds this.
    metadata_ceiling_pct_of_fast: u8,
    /// Per-tier minimum viable budget (per-node share floor) for
    /// fjall keyspace bring-up. Default 256 MiB — phase 3 measures
    /// the actual keyspace overhead on the existing fjall stores
    /// and re-tunes. Below this floor, fjall can't initialize a
    /// keyspace (memtable + WAL minimums). Per Q10 resolution:
    /// BestEffort mode redistributes the floor from largest-share
    /// nodes; Strict mode rejects the policy.
    per_tier_min_viable_bytes: u64,
}

impl WorkloadParams {
    /// Per-cluster metadata bytes per file. Composes the
    /// single-replica footprint with the replication factor.
    ///
    /// **Rev 4 ownership fix**: `PER_FILE_METADATA_FOOTPRINT_BYTES`
    /// moves from `crates/kiseki-server/src/system_disk.rs` (a
    /// binary-only crate) to `crates/kiseki-common/src/metadata.rs`
    /// (a library crate every dependent crate can import). Rev 3
    /// drafted this method against `kiseki_server::system_disk::*`
    /// which would have failed to compile because `kiseki-server`
    /// has no `lib.rs` target. Phase 1 of the implementation moves
    /// the constant + updates the two existing call sites
    /// (`runtime.rs:529`, `web/admin_extra.rs:1635`) to import from
    /// `kiseki_common::metadata::PER_FILE_METADATA_FOOTPRINT_BYTES`.
    /// ADR-030 §3 documentation is amended accordingly.
    fn metadata_per_file_bytes(&self) -> u64 {
        u64::from(self.metadata_replication)
            * kiseki_common::metadata::PER_FILE_METADATA_FOOTPRINT_BYTES
    }
}
```

Default policy (no operator config). Metadata tiers (IntentStore,
CompositionMeta, ChunkMeta) collectively form the "Metadata"
capacity slot — they share one `Auto` entry. RaftLog is
`BootstrapOnly` per §D2.5 (env-var-driven path). Chunks consume
the remainder per `TierCapacity::Remainder`.

| Tier | Preferences (best-effort) | Capacity |
|---|---|---|
| SmallObject | `Class(Nvme), Class(Ssd), DataDir` | `Auto{ pct: 80, floor: 50 GiB × N }` |
| IntentStore | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| **RaftLog** | **`KISEKI_RAFT_LOG_DIR` env var (defaults to `KISEKI_DATA_DIR/raft`)** | **`BootstrapOnly` — never resolver-routed; see §D2.5** |
| CompositionMeta | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| ChunkMeta | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| Metadata (synthetic — sum of IntentStore + CompositionMeta + ChunkMeta) | (resolves through member tiers) | `Auto{ pct: 30 (ceiling), floor: 10 GiB × N_nodes }` |
| (LSM headroom — not a real tier; reserved unallocated) | (on whichever class above lands) | `fast_headroom_pct = 25` |
| Chunks (raw block, orthogonal axis) | `KISEKI_RAW_DEVICES` (existing) | `Remainder` (slow tier + fast spillover) |

All metadata wants fast media; chunk fragments stay on
`KISEKI_RAW_DEVICES` (orthogonal). Best-effort by default: a node
with only HDD warns loudly but still serves.

Strict mode is for production fleets where any node missing the
declared tier should refuse to start (deployment regression catch).

### D4.5. Capacity allocation formula

The capacity question — "how much of each device class goes to
which tier?" — is **cluster-aggregate**, not per-node. A
heterogeneous cluster (e.g. one node with 200 GiB root NVMe, five
nodes with 1.5 TiB local NVMe each) needs cluster-wide budget
targets that distribute proportionally to each node's fast-tier
share. Flat per-node percentages would give the 200 GiB-NVMe node
the same metadata budget as the 1.5 TiB-NVMe nodes and starve
small-file capacity cluster-wide.

#### Inputs

From the catalog (D2), the resolver sums across every node's
inventory **in ascending `NodeId` order** (canonical — see
I-DI10):

```
F_total = Σ_{n in inventories.keys() sorted asc} n.fast_capacity   # NVMe-class
S_total = Σ_{n in inventories.keys() sorted asc} n.slow_capacity   # SSD + HDD
N_nodes_with_fast = | nodes with fast_capacity > 0 |
N_nodes_total     = | inventories | (all nodes, including F=0)
```

The `Σ` order matters: float64 addition is not associative, so two
nodes summing the same set in different orders can produce results
that differ by one ULP. Canonical ascending-`NodeId` order
guarantees bit-exact determinism across every node (I-DI10).

From `PlacementPolicy.workload` (defaults shown — `R_metadata=3`
default makes `metadata_per_file_bytes = 1536`):

```
avg_file_bytes               = 256 KiB
metadata_replication         = 3
metadata_per_file_bytes      = R_metadata × system_disk::PER_FILE_METADATA_FOOTPRINT_BYTES
                             = 3 × 512 B
                             = 1.5 KiB
growth_headroom              = 1.5
fast_headroom_pct            = 25        # rev 3 — was 10; LSM compaction floor
metadata_ceiling_pct_of_fast = 30
per_tier_min_viable_bytes    = 256 MiB
```

#### Cluster-wide budgets

```
# Edge case: no fast tier anywhere in the cluster (all root disk,
# all spinning, dev/test). Re-base every fjall budget on S_total
# and warn loudly. Without this branch the formula returns zero
# budgets cluster-wide and capacity tracking goes dark (auditor F-5).
if F_total == 0:
    floor          = per_tier_min_viable_bytes × N_nodes_total
    metadata_budget   = clamp(projected_metadata × growth_headroom,
                              floor,
                              ceiling = (metadata_ceiling_pct_of_fast / 100) × S_total)
    headroom_budget   = 0     # no LSM reserve when no fast tier; on
                              # spinning storage the I/O queue is
                              # the bottleneck, not headroom
    small_file_budget = clamp(0.20 × S_total,        # ~20% of S
                              floor,
                              ceiling = S_total − metadata_budget)
    chunk_budget      = S_total − metadata_budget − small_file_budget
    log.warn("ADR-049: F_total=0 — fjall metadata + small files \
              fall back to slow tier; throughput will be capped \
              by spinning/SATA I/O queue. See ADR-049 §D4.5 \
              `F_total == 0` branch.")
    return ClusterBudgets { metadata_budget, headroom_budget,
                            small_file_budget, chunk_budget }

# Normal path: F_total > 0.
# Projected file count from chunk capacity, NOT fast capacity —
# metadata grows with the bulk store, not with the fast tier.
projected_files       = S_total / avg_file_bytes

# Cluster-wide metadata bytes needed, with growth headroom.
projected_metadata    = projected_files × metadata_per_file_bytes

# Clamp to floor (cold-start floor) and ceiling (don't starve
# SmallObject). Floor scales with N_nodes_total since per-node
# distribution shrinks share with cluster size; small-node share
# must clear `per_tier_min_viable_bytes` (Q10).
metadata_budget       = clamp(
                          projected_metadata × growth_headroom,
                          floor   = max(10 GiB × N_nodes_total,
                                        per_tier_min_viable_bytes × N_nodes_total),
                          ceiling = (metadata_ceiling_pct_of_fast / 100) × F_total
                        )

# LSM compaction reserve — 25% of F_total by default (rev 3).
headroom_budget       = (fast_headroom_pct / 100) × F_total

# Small file gets the remainder of the fast tier.
small_file_budget     = max(0, F_total − metadata_budget − headroom_budget)

# Chunks consume slow tier + any unused fast tier. The fast-leftover
# clause is identically zero by construction (small_file_budget already
# absorbs F_total − metadata − headroom), but kept explicit so the
# auditor verifies the algebra rather than the author's claim.
chunk_budget          = S_total + max(0, F_total − metadata_budget
                                          − headroom_budget
                                          − small_file_budget)

# Apply-time cluster-aggregate Absolute pre-check (I-DI9). Runs at
# `SetPlacementPolicy` apply, BEFORE per-node distribution, so an
# operator pushing `Absolute { cluster_bytes: 100 TiB }` on a
# 10 TiB-fast cluster is rejected at the LogCommand apply lock
# with a clear error rather than per-node clamping after the fact.
assert Σ_{t in Absolute tiers} t.cluster_bytes
       ≤ F_total − headroom_budget,
       else error PlacementPolicyOvercommit
```

#### Per-node distribution

```
# Phase 1: naive proportional distribution.
for each node n in inventories (ascending NodeId order):
    if n.fast_capacity > 0:
        share = n.fast_capacity / F_total
        n.metadata_share    = metadata_budget    × share
        n.small_file_share  = small_file_budget  × share
        n.headroom_share    = headroom_budget    × share
    else:
        n.metadata_share    = 0
        n.small_file_share  = 0
        n.headroom_share    = 0
    n.chunk_share = n.slow_capacity + leftover_fast_on_n

# Phase 2: Q10 per-node-floor redistribution (rev 4 fix).
# After phase 1, some nodes may have shares below
# `per_tier_min_viable_bytes` (default 256 MiB) — fjall can't
# bring up a keyspace below that. Redistribute the shortfall
# from the largest-share donor nodes, deterministically.
for each tier T in [Metadata, SmallObject, Headroom]:
    # Iterate in canonical (ascending NodeId) order for determinism.
    for each node n with n.fast_capacity > 0:
        if n.{T}_share < per_tier_min_viable_bytes:
            shortfall = per_tier_min_viable_bytes − n.{T}_share

            if mode == Strict:
                # Strict mode rejects the policy at the
                # `SetPlacementPolicy.apply` gate (I-DI9 + I-DI8).
                # `compute_cluster_budgets` returns
                # `Err(PlacementError::BelowMinViable)`.
                return Err(BelowMinViable { tier: T, node_id: n,
                                            share_bytes: n.{T}_share,
                                            min_viable: per_tier_min_viable_bytes });
            else:
                # BestEffort: take from donors. Donors are nodes
                # whose share for THIS tier is strictly above
                # per_tier_min_viable_bytes, sorted by descending
                # `n.{T}_share` then ascending NodeId (canonical
                # tie-break). Largest donor first.
                donors = inventories
                    .filter(|d| d.fast_capacity > 0 && d.NodeId != n.NodeId
                               && d.{T}_share > per_tier_min_viable_bytes)
                    .sort_by(|d| (-d.{T}_share, d.NodeId));

                # Each donor contributes proportionally to its
                # excess-above-floor, capped at the shortfall.
                total_excess = Σ donors.{T}_share − per_tier_min_viable_bytes
                for donor in donors:
                    donor_excess = donor.{T}_share − per_tier_min_viable_bytes
                    donor_contribution = (donor_excess / total_excess) × shortfall
                    donor.{T}_share -= donor_contribution

                n.{T}_share = per_tier_min_viable_bytes

                # Emit observability:
                metric kiseki_capacity_redistributed_bytes{tier=T,
                       from=donor.NodeId, to=n.NodeId} += donor_contribution

# After phase 2, every node either has {T}_share ≥
# per_tier_min_viable_bytes (BestEffort), or the policy was
# rejected at apply (Strict). I-DI8 still holds: the cluster
# aggregate per-tier budget is unchanged (we only moved bytes
# between nodes).
```

A node with zero fast tier hosts no metadata or small-file budget
— other nodes carry its share. Cluster aggregate budgets stay
satisfied.

The phase-2 redistribution is purely deterministic (canonical
order, deterministic donor selection, deterministic contribution
formula) so I-DI10 holds across the algorithm.

#### Edge cases

| Cluster shape | F_total | S_total | Behavior |
|---|---|---|---|
| All-NVMe (no slow tier) | high | 0 | `metadata_budget` → floor (`10 GiB × N_nodes`); chunks land on F leftover |
| Slow-only (no NVMe anywhere) | 0 | high | warn loudly; metadata + small fall back to S per ADR-049 best-effort mode |
| NVMe-root + disks (one node) | partial F | high | metadata + small concentrate on the NVMe-holding nodes; root-NVMe node carries its 1/F_total share |
| NVMe fast + NVMe slow per node | mixed F via `KISEKI_DEVICE_TAGS` | high | operator tags fast NVMe → Metadata tier prefs target `Tag("nvme-fast")`; slow NVMe → SmallObject prefs target `Tag("nvme-slow")`; chunks → S |
| Tiny cluster, low chunk total | low | low | `metadata_budget = floor` (`10 GiB × N_nodes`); SmallObject takes most of F |
| Huge file workload (avg = 10 MiB) | normal | huge | `projected_metadata` stays small; `metadata_budget` hits floor; SmallObject gets ~89% of F |
| Small-file workload (avg = 32 KiB) | normal | normal | `projected_metadata` grows; ceiling at 30% of F_total caps it before starving SmallObject |
| New node joins (rebalance) | F_total grows | S_total grows | cluster budgets recompute; per-node shares re-derive; if any node's budget shrinks below current usage → migration alert per §D8 |

#### Worked example — homogeneous 6-node lssd

6 × `c3-standard-22-lssd`, each with 1.5 TiB NVMe + 8 TiB SATA
(counted as slow). Defaults applied. **All capacities in binary
units** (1 TiB = 1024 GiB; the earlier rev 2 example silently
slipped into decimal for some intermediates — corrected here).

```
F_total            = 6 × 1.5 TiB = 9 TiB = 9216 GiB
S_total            = 6 × 8 TiB   = 48 TiB = 49152 GiB
N_nodes_with_fast  = 6
N_nodes_total      = 6
projected_files    = 49152 GiB / 256 KiB
                   = 49152 × 2^30 / (256 × 2^10)
                   = 49152 × 2^20
                   = 51 539 607 552 / 256
                   = 201 326 592 ≈ 201.33 M files exact
projected_metadata = 201 326 592 × 1536 B = 309 237 645 312 B
                   = 309 237 645 312 / 2^30 GiB
                   = **287.987 GiB**                                        (rev 4 fix: was 309.1)
metadata_budget    = clamp(287.987 × 1.5 = 431.98 GiB,
                           floor = max(60 GiB, 1.5 GiB) = 60 GiB,
                           ceiling = 0.30 × 9216 = 2764.8 GiB) = **431.98 GiB**   (~4.7% of F_total)
headroom_budget    = 0.25 × 9216 GiB = 2304.0 GiB                            (rev 3: 25%)
small_file_budget  = 9216 − 431.98 − 2304.0 = **6480.02 GiB ≈ 6.328 TiB**    (~70.3% of F_total)
chunk_budget       = 48 TiB SATA + 0 fast spillover

per node (share = 1500 / 9216 = exactly 16.6667% each):
  metadata_share   = 431.98 × 0.166667 = **71.997 GiB**
  small_file_share = 6480.02 × 0.166667 = **1080.00 GiB ≈ 1.055 TiB**
  headroom_share   = 2304.0 × 0.166667 = **384.00 GiB**
  chunk_share      = 8 TiB SATA per node

Verification per I-DI8 (per-node sum ≤ node.fast_capacity):
  71.997 + 1080.00 + 384.00 = 1535.997 GiB ≈ 1.5 TiB (1536 GiB) ✓
  Difference of 0.003 GiB = 3.2 MiB is float-rounding noise; the
  algebraic identity Σ shares = 1.0 holds exactly.
```

(Rev 4 corrected the residual GB-vs-GiB drift the rev 3 auditor
caught: `projected_metadata` was labeled GiB while computed in
decimal billions, cascading through `metadata_budget` and
`small_file_budget`. Implementer's property test consumes the
corrected numbers as oracle.)

Note: rev 3's 25% LSM headroom moves SmallObject from ~85% to
~70% of F_total. Phase 3 measurement of actual fjall compaction
overhead may revise both `fast_headroom_pct` and this allocation.

#### Worked example — heterogeneous, one root-NVMe node

Same cluster, but **node 4 has only 200 GiB root NVMe** (no lssd).
Auditor-corrected arithmetic (rev 2 had 19.5% which is wrong;
real value is 19.04% = 1500/7880).

```
F_total = 200 GiB + 5 × 1.5 TiB = 200 + 5 × 1536 GiB = 7880 GiB ≈ 7.7 TiB
N_nodes_with_fast = 6
N_nodes_total     = 6

projected_files    = 48 TiB / 256 KiB ≈ 201.3 M
projected_metadata = 201.3 M × 1.5 KiB ≈ 309.1 GiB
metadata_budget    = clamp(309.1 × 1.5 = 463.6 GiB,
                           floor = 60 GiB,
                           ceiling = 0.30 × 7880 = 2364.0 GiB) = 463.6 GiB
headroom_budget    = 0.25 × 7880 GiB = 1970.0 GiB                              (rev 3: 25%)
small_file_budget  = 7880 − 463.6 − 1970.0 = 5446.4 GiB ≈ 5.32 TiB             (~69% of F_total)

shares (exact):
  node 4:        200 / 7880 = 2.538%
  nodes 1,2,3,5,6: 1500 / 7880 = 19.036% each
  sum = 2.538 + 5 × 19.036 = 97.718 + 2.538 = ...
  ... wait: 5 × 19.036 = 95.180; 95.180 + 2.538 = 97.718? No — by
  construction Σ shares = (200 + 5×1500) / 7880 = 7700/7880 = 97.71%.
  The 2.29% residue belongs to nodes 1-6's `slow_capacity` not
  counted in F_total. Shares sum to 1.0 IF AND ONLY IF the
  numerator and denominator both reference the same set (here:
  fast_capacity), which they do. The "100% sum" check is on
  fast_share, not on (fast+slow)_share. Σ_n n.fast/F_total = 1.0
  exactly by definition.

  Confirming: 200/7880 + 5×1500/7880 = (200 + 7500)/7880 = 7700/7880
  ≠ 1.0. **The first arithmetic was wrong**. F_total counts ALL
  nodes' fast capacity; node 4's is 200 GiB; nodes 1-3,5,6 each
  have 1500 GiB. Sum = 200 + 5 × 1500 = 7700 GiB. **Not 7880.**
  Recomputing:
    F_total = 7700 GiB (not 7880).
    node 4 share = 200/7700 = 2.597%
    nodes 1,2,3,5,6 share = 1500/7700 = 19.481% each
    sum = 2.597 + 5 × 19.481 = 99.999% ≈ 100% ✓

(rev 3 arithmetic corrected; rev 2 used 7880 GiB which double-
counted node 4's 200 GiB as if it were ADDED to the 5 × 1500. The
correct cluster total is 5 × 1500 + 200 = 7700 GiB.)

Continuing with F_total = 7700 GiB and **rev 4-corrected
projected_metadata = 287.987 GiB**:
  metadata_budget          = clamp(287.987 × 1.5 = 431.98 GiB,
                                   floor = 60 GiB,
                                   ceiling = 0.30 × 7700 = 2310 GiB) = **431.98 GiB**
  headroom_budget          = 0.25 × 7700 = 1925.0 GiB
  small_file_budget        = 7700 − 431.98 − 1925.0 = **5343.02 GiB ≈ 5.218 TiB**

  node 4 (share 200/7700 = 2.5974%):
    metadata  = 431.98 × 0.025974 = **11.22 GiB**
    small     = 5343.02 × 0.025974 = **138.79 GiB**
    headroom  = 1925.0 × 0.025974 = **49.99 GiB**
    sum                              = **200.00 GiB** ✓ (exactly fits 200 GiB NVMe)

  node 1 (share 1500/7700 = 19.4805%):
    metadata  = 431.98 × 0.194805 = **84.16 GiB**
    small     = 5343.02 × 0.194805 = **1040.84 GiB ≈ 1.017 TiB**
    headroom  = 1925.0 × 0.194805 = **374.99 GiB**
    sum                              = **1500.00 GiB** ✓ (exactly fits 1.5 TiB lssd)
```

Node 4 still participates with proportionally smaller shares. The
cluster aggregate budgets are still satisfied. Per-node sum equals
node fast capacity exactly (I-DI8 ✓ — the identity Σ_n share_n = 1
makes this hold by construction).

(Rev 4 corrected line items 12.04/137.94/90.31/1034.74 → exact
values 11.22/138.79/84.16/1040.84. The per-node sum stays 200.00
/ 1500.00 GiB either way — the algebraic identity hides the
intermediate error, which is what made the bug survive rev 2 and
rev 3 review. Implementer's property test consumes the corrected
line items as oracle.)

**Why this matters**: the rev 2 example had ~7% of arithmetic
slop. Real implementations need bit-exact agreement across nodes
(I-DI10). Implementer MUST use the canonical
`Σ_{n sorted asc by NodeId}` formula AND the corrected per-node
math from this example as the property-test reference.

#### Determinism (I-DI7)

The formula is pure: given the same `(F_total, S_total, N_nodes,
WorkloadParams)` snapshot, every node MUST derive the same
cluster-wide budgets. Per-node shares are then computed from the
node's local `fast_capacity / F_total` ratio. No time, no random
input. The resolver is re-runnable for "what-if" admin previews
without touching cluster state.

### D5. Per-node path + budget resolver

At boot, after every node's inventory is published and the policy
is read from the catalog, each node runs the resolver for every
fjall tier. The resolver produces both a path AND a budget per
tier:

```rust
fn resolve_tier(
    catalog: &ClusterDeviceCatalog,    // all-node inventories + policy
    local_node_id: NodeId,
    tier: FjallStoreTier,
) -> Result<ResolvedTierBudget, PlacementError>;

struct ResolvedTierBudget {
    tier: FjallStoreTier,
    chosen_mount: PathBuf,
    chosen_class: MediaType,
    /// Per §D4.5 — cluster_budget × this_node_share.
    budget_bytes: u64,
}
```

Algorithm:

1. **Path** (preferences walk, unchanged from rev 1):
   1. Walk `policy.preferences` left-to-right.
   2. For each matcher, find the first `DeviceEntry` in the local
      node's inventory whose `media_class` / `tag` matches.
   3. If found, the path is `entry.mount_path / "kiseki" / tier_name`.
   4. If none match: `Strict` returns
      `PlacementError::NoMatchingDevice`; `BestEffort` falls back to
      `data_dir / tier_name` and emits a warning.

2. **Budget** (§D4.5 formula, projected onto this node):
   1. Sum `F_total`, `S_total`, `N_nodes` across the catalog.
   2. Apply the §D4.5 formula → cluster `metadata_budget`,
      `small_file_budget`, `headroom_budget`, `chunk_budget`.
   3. Compute `local_share = local.fast_capacity / F_total`.
   4. `budget_bytes = cluster_<tier>_budget × local_share`.

3. **Per-node inventory update**: write `resolved_budgets` back
   into the local `NodeDeviceInventory` and republish via the
   normal inventory path. The catalog's `resolved_budgets` is
   advisory (used by `kiseki-admin topology node-inventory show`)
   — the authoritative budget is what the resolver computes at
   boot from the policy + inventories.

Per-tier subdirectory under each device path: `"kiseki/<tier>"`,
e.g. `/mnt/nvme0/kiseki/small-object`. Predictable enough for
operators to `du -sh /mnt/nvme0/kiseki/*` and see per-tier usage.

Re-resolution: the resolver runs at boot. Mid-run policy changes
trigger D8 migration; mid-run inventory changes (a node's free
bytes drift) do NOT re-resolve — budgets are fixed at boot to
avoid live-rebalance complexity. The catalog still gets the
refreshed free-bytes via D3's periodic publish so observability
gauges (D10) show drift.

### D5.5. Resolver interfaces (rev 3)

The rev-3 architect review flagged that rev 2 had data models but
no trait signatures. The implementer needs these contracts before
phase 3:

```rust
// crates/kiseki-server/src/cluster_control/device_catalog.rs (new)

/// Reader contract every fjall consumer uses at boot.
/// Cached on `Arc<dyn DeviceResolver>`. Three implementations:
///   - `ControlPlaneDeviceResolver` (reads catalog snapshot)
///   - `FallbackDeviceResolver` (cache file; used on catalog timeout)
///   - `TestDeviceResolver` (hand-built inventory + policy)
pub trait DeviceResolver: Send + Sync {
    fn resolve_tier(&self, tier: FjallStoreTier)
        -> Result<ResolvedTierBudget, PlacementError>;
    /// All four catalog-routed tiers in one call (SmallObject,
    /// IntentStore, CompositionMeta, ChunkMeta). RaftLog is
    /// bootstrap-only (§D2.5) and NOT included.
    fn resolve_all(&self) -> Result<[ResolvedTierBudget; 4], PlacementError>;
    fn policy_revision(&self) -> u64;
    fn local_node_id(&self) -> NodeId;
}

/// Read side of the control-plane catalog. Mirrors the existing
/// `NamespaceShardMapStore` pattern (state_machine.rs:304).
pub trait DeviceCatalogRead {
    fn snapshot(&self) -> ClusterDeviceCatalog;
    fn inventories(&self) -> BTreeMap<NodeId, NodeDeviceInventory>;
    fn policy(&self) -> PlacementPolicy;
    fn workload(&self) -> WorkloadParams;
}

/// Boot-time "wait for catalog" — NEW pattern (not in current
/// codebase; rev 4 honest correction — the rev 3 "mirrored from
/// runtime.rs:782-797" claim referred to *construction*, not a
/// wait-for-ready primitive). Returns once:
///   (a) the local node's `UpsertNodeInventory` has applied AND
///   (b) the catalog's `policy_change_ms` (NOT `inventory_change_ms`)
///       has been stable for >= `quiescence_ms`.
/// The split clock (rev 4 fix for re-diamond N-5) ensures inventory
/// refresh churn from peer nodes doesn't reset quiescence —
/// otherwise a busy cluster's `policy_change_ms` is stable but
/// `inventory_change_ms` resets every ~600 ms (100 nodes × 60 s
/// refresh) and quiescence never fires. Default `quiescence_ms`
/// is now 30 s (was 120 s); `timeout` raised to 90 s so timeout
/// > quiescence by a 3× margin.
pub async fn await_catalog_ready(
    ctrl: &OpenRaftControlStore,
    local_node_id: NodeId,
    timeout: Duration,            // KISEKI_CATALOG_BOOT_TIMEOUT_MS default 90 s (rev 4: was 30)
    quiescence_ms: Duration,      // KISEKI_CATALOG_QUIESCENCE_MS  default 30 s (rev 4: was 120)
) -> Result<Arc<dyn DeviceResolver>, PlacementError>;

/// Periodic inventory refresh task. Spawned once per node from
/// runtime.rs after the resolver is built. Mirrors the composition-
/// store flusher (runtime.rs:1780-1790).
pub struct InventoryReporter {
    ctrl_store: Arc<OpenRaftControlStore>,
    node_id: NodeId,
    interval: Duration,            // KISEKI_INVENTORY_REFRESH_MS default 60 s
    metrics: Arc<KisekiMetrics>,
}
impl InventoryReporter {
    pub fn spawn(self) -> tokio::task::JoinHandle<()>;
    async fn tick(&mut self) -> Result<(), PlacementError>;  // I-DI6 idempotent
}

/// Pure §D4.5 formula — split out so `topology capacity-policy
/// preview` (§D7) can call it without touching cluster state.
/// I-DI7 + I-DI10 both gate on its determinism. Implementer
/// MUST use canonical ascending-NodeId summation order.
pub fn compute_cluster_budgets(
    inventories: &BTreeMap<NodeId, NodeDeviceInventory>,
    workload: &WorkloadParams,
    policy: &PlacementPolicy,
) -> Result<ClusterBudgets, PlacementError>;

/// Per-node projection of `ClusterBudgets`. Pure function.
pub fn distribute_to_node(
    cluster: &ClusterBudgets,
    local_node: &NodeDeviceInventory,
    f_total: u64,                  // pass through to avoid re-summing
) -> NodeBudgets;
```

`PlacementError` lives in `crates/kiseki-server/src/cluster_control/`:

```rust
pub enum PlacementError {
    NoMatchingDevice { tier: FjallStoreTier, mode: PolicyMode },
    PolicyOvercommit { tier: FjallStoreTier, node_id: NodeId,
                       cluster_demand: u64, cluster_available: u64 },
    BelowMinViable    { tier: FjallStoreTier, node_id: NodeId,
                        share_bytes: u64, min_viable: u64 },
    CatalogBootTimeout { elapsed: Duration },
    CatalogStale       { last_change_ms_age: Duration },
    Io(io::Error),
}
```

### D6. Runtime wiring

`runtime.rs` boot sequence changes:

1. Initialize local inventory (D1).
2. Open control-plane Raft handle.
3. Submit `UpsertNodeInventory`, wait for commit.
4. Read `policy` from local catalog state machine.
5. For each fjall store, call `resolve_path(...)` to get the
   directory. Open the fjall keyspace at that path.

The existing `KISEKI_DATA_DIR` becomes the **last-resort fallback**
in the policy (`DataDir` matcher) and the root of any local
operational files that aren't fjall stores (e.g. cache, temp).
Operator can still rely on `KISEKI_DATA_DIR` for single-disk
deployments — the default policy still resolves to it when no
faster device exists.

### D7. Admin surface

New admin commands (extend `kiseki-admin topology`). Three knob
classes, in order of bluntness — operator usually starts with
**workload tuning** (the highest-level lever) and only drops down
to per-tier overrides when needed:

```
# (1) Workload assumption — drives the §D4.5 metadata projection.
kiseki-admin topology workload show
kiseki-admin topology workload set                         \
    [--avg-file-bytes=256KiB]                              \
    [--metadata-per-file-bytes=1.5KiB]                     \
    [--growth-headroom=1.5]                                \
    [--fast-headroom-pct=10]                               \
    [--metadata-ceiling-pct-of-fast=30]

# (2) Per-tier capacity override (caps the auto-formula).
kiseki-admin topology capacity-policy show
kiseki-admin topology capacity-policy set-tier             \
    --tier=<SmallObject|Metadata|...>                      \
    --capacity=<auto|absolute>                             \
    [--pct=<n>] [--floor=<bytes>] [--ceiling=<bytes>]      \
    [--bytes=<n>]              # only when --capacity=absolute

# (3) Per-tier preferences + mode (§D4 placement, unchanged from rev 1).
kiseki-admin topology placement-policy show
kiseki-admin topology placement-policy set --from-file=<yaml>
kiseki-admin topology placement-policy set-tier --tier=<name> \
    --prefer=<class-or-tag>,<class-or-tag>,... [--mode=strict|best-effort]

# (4) Inventory inspection — operator sees per-node resolved budgets.
kiseki-admin topology node-inventory list
kiseki-admin topology node-inventory show --node=<id>

# Sample output of `node-inventory show --node=4`:
#
#   node 4 (3 devices):
#     /mnt/nvme0   nvme   1.8 TiB  (1.6 TiB free, exclusive)
#     /mnt/sata0   ssd    3.6 TiB  (3.4 TiB free, exclusive)
#     /            hdd    100 GiB  (50 GiB free, shared)
#
#   resolved budgets (formula: cluster_metadata=429 GiB × share):
#     Metadata     → /mnt/nvme0/kiseki/metadata     180 GiB   (share 41.9%)
#     SmallObject  → /mnt/nvme0/kiseki/small-object 1.44 TiB  (share 41.9%)
#     headroom     reserved                          180 GiB
#     Chunks       → /mnt/sata0/kiseki/chunks       3.6 TiB   (Remainder)

# (5) What-if preview — runs the resolver against a hypothetical
# inventory/policy without applying. Useful for sizing.
kiseki-admin topology capacity-policy preview              \
    [--policy-file=<yaml>]                                 \
    [--workload-file=<yaml>]                               \
    [--inventory-file=<yaml>]
```

The HTTP surface mirrors:
- `GET  /admin/topology/devices` — full catalog
- `GET  /admin/topology/devices/<node-id>` — single node + resolved budgets
- `GET  /admin/topology/placement-policy` — current policy
- `PUT  /admin/topology/placement-policy` — replace policy
- `GET  /admin/topology/workload` — current workload params
- `PUT  /admin/topology/workload` — update workload params
- `POST /admin/topology/capacity-policy/preview` — what-if

### D8. Reshape / migration story

`kiseki-admin storage migrate` is a **mount-path-change** primitive
— it moves a tier from one mount point to another mount point
(operator policy edit; disk replacement; relocation onto a freshly-
provisioned VG). It is **NOT** the tool for capacity expansion —
that's an OS-layer LVM operation (see §D8.2).

v1 approach (operator-driven):

1. Operator sets new policy via admin RPC, OR provisions a new
   mount point and updates `KISEKI_DEVICE_TAGS`.
2. Each affected node logs: "policy says SmallObject should live on
   `/mnt/fast-small-vg2` but I'm currently on `/mnt/fast-small-vg1`
   — drain via: `kiseki-admin storage migrate --tier=small-object
   --node=N`".
3. Migration is a separate explicit command that:
   - Quiesces writes to the tier (sets it read-only via a state
     machine bit).
   - Copies the fjall directory from old path to new path
     atomically (`rsync` or fjall-native snapshot+restore).
   - Re-opens the keyspace at the new path.
   - Clears the read-only bit.
4. Operator runs migration per-node, staggered, with cluster
   redundancy keeping the tier available.

v2 (deferred): automatic migration on policy change. Requires more
careful quiesce-and-resume coordination across replicas; punted
until v1 ships.

If a node is **added** to the cluster, it picks up the current
policy from the catalog at boot and opens its fjall stores at the
correct paths from day one. No migration needed.

If **capacity** on an existing mount is exhausted, the operator
performs an online LVM extension on that mount — no kiseki
involvement, no migrate, no pointer-file update. See §D8.2.

### D8.1. Path-move semantics (rev 3 — F-3 / I-CP-Move)

The rev-3 adversary review surfaced a silent data-availability
loss: an operator changes the placement policy and reboots a node
without running `kiseki-admin storage migrate`. The hydrator opens
an empty fjall keyspace at the new resolved path, resumes from
`meta.last_applied_seq = 0`, and re-applies the openraft log
compaction window. **If the log was compacted past the orphan
keyspace's `last_applied_seq`, that node's compositions vanish
from local reads** (other replicas still serve, masking the
issue). ADR-040 I-CP1 is violated through this path.

Resolution: each fjall consumer writes a small **path-version
pointer file** under `KISEKI_DATA_DIR` (a survival-anchor that
exists OUTSIDE the catalog-resolved tree):

```
$KISEKI_DATA_DIR/kiseki-tier-paths.json
{
  "small_object":    "/mnt/nvme0/kiseki/small-object",
  "intent_store":    "/mnt/nvme0/kiseki/intent-store",
  "composition_meta":"/mnt/nvme0/kiseki/composition-meta",
  "chunk_meta":      "/mnt/nvme0/kiseki/chunk-meta",
  "raft_log":        "/data/kiseki/raft"
}
```

Algorithm at boot, per tier (run as part of resolver finalization,
BEFORE any fjall open):

1. Read `kiseki-tier-paths.json`. If missing (first boot),
   write the resolved path and proceed.
2. If `prior_path == resolved_path`, proceed normally.
3. If `prior_path != resolved_path`:
   - Check whether `prior_path` exists and is non-empty
     (`fjall::Database::open(prior_path).keyspace_count() > 0`).
   - If non-empty: **REFUSE TO OPEN** at `resolved_path`. Emit
     `error!` with both paths + the `kiseki-admin storage
     migrate` command the operator must run. Exit 1.
   - If empty / missing: this is a clean policy adopt (operator
     ran migrate first; or first boot). Update the pointer file,
     proceed.
4. After successful fjall open at `resolved_path`, update the
   pointer file atomically.

`kiseki-admin storage migrate` (§D8) atomically: (i) quiesces the
tier; (ii) copies the fjall directory `prior → resolved`; (iii)
updates `kiseki-tier-paths.json`; (iv) clears quiesce.

**New invariant I-CP-Move:** a fjall keyspace MUST NOT be opened
at a new resolved path while a non-empty keyspace exists at the
prior resolved path recorded in `kiseki-tier-paths.json`. Enforced
in `runtime.rs` boot sequence by reading the pointer file BEFORE
calling each fjall consumer's open. ADR-040 I-CP1's hydrator
contract is preserved by this gate.

**I-CP-Move scope (rev 5 clarification):** the guard fires only
when the *mount path itself* changes — i.e. when `prior_path` and
`resolved_path` differ as strings. **It does NOT fire for
capacity-expansion operations on the same mount** (see §D8.2):
`vgextend` + `lvextend` + `resize2fs` grow the backing storage
under `/mnt/<name>/` without changing the path, the pointer file,
or the fjall keyspace location. The guard distinguishes:

- "operator moved SmallObject from `/mnt/old-vg` to `/mnt/new-vg`" →
  I-CP-Move trips; migrate required.
- "operator added 4 NVMe to `/mnt/fast-small`'s VG and resized the
  LV" → mount path unchanged; pointer unchanged; I-CP-Move silent;
  fjall sees the bigger `statvfs()` on next compaction.

### D8.2. Fast-tier mount aggregation (rev 5 — LVM convention)

The per-tier mount the resolver picks (e.g. `/mnt/fast-small`,
`/mnt/fast-meta`) is **always backed by an LVM logical volume**,
even on single-disk deployments. The setup script provisions:

```
N × physical NVMe  →  N × LVM PVs  →  one VG per fast tier  →
                  →  one striped LV per fast tier (-i N -I 64k)  →
                  →  ext4 mount at /mnt/<tier>
```

Default `N=1` per tier (a degenerate single-disk-stripe LV) so
small deployments still pay the LVM tax (negligible — ~1-2 µs per
I/O for the dm-stripe target) but inherit the same capacity-
expansion shape as large deployments: there is no "first disk"
special case.

**Why LVM and not raw fjall-on-block:** fjall is FS-bound — its
SST files, manifest, and WAL go through `std::fs`. A raw-block
fjall would require a fjall fork with a custom storage backend
(out of scope). For the per-tier workloads (small KV ops on
metadata, KB-sized objects on SmallObject), the FS / LVM
overhead is negligible — LSM compaction dominates. Raw block
remains the right shape for the chunk pool (large opaque blobs,
offset/length addressing, `KISEKI_RAW_DEVICES` — §D11.1 axis,
unchanged by this revision).

**Capacity expansion runbook** (online, no service restart):

```bash
# Operator attaches 3 new local NVMe to a node:
pvcreate /dev/nvme7n1 /dev/nvme8n1 /dev/nvme9n1
vgextend kiseki_small /dev/nvme7n1 /dev/nvme8n1 /dev/nvme9n1
lvextend -l +100%FREE /dev/kiseki_small/data
resize2fs /dev/kiseki_small/data
# Done. fjall's statvfs reports the new total; the §D4.5 formula
# re-balances on next `compute_cluster_budgets` invocation
# (every InventoryReport apply tick); the resolver re-picks the
# same mount; pointer file unchanged; I-CP-Move silent.
```

**Why striped LVs (`-i N -I 64k`):** uniform 64 KiB stripe across
all PVs in the VG. SmallObject and the three metadata keyspaces
all benefit from disk-level parallelism on LSM compaction and
WAL fsync. The stripe width is chosen at LV-create time; growing
the LV onto new PVs uses linear segments by default (writes after
extension hit the new PVs only). For full re-striping after a
large extension, the operator runs `lvconvert --type striped
--stripes N` offline — that IS a path-affecting operation and
goes through `kiseki-admin storage migrate` (back to a fresh LV).

**Per-tier sizing convention:**

| Tier            | LVM VG          | LV name | LV mount        |
|-----------------|-----------------|---------|-----------------|
| SmallObject     | `kiseki_small`  | `data`  | `/mnt/fast-small` |
| All 3 metadata  | `kiseki_meta`   | `data`  | `/mnt/fast-meta`  |

The three metadata tiers share one mount (`/mnt/fast-meta`) and
one LV — they're co-located on the same backing storage but in
distinct fjall keyspaces under `/mnt/fast-meta/kiseki/intent-store/`,
`/mnt/fast-meta/kiseki/composition-meta/`, `/mnt/fast-meta/kiseki/
chunk-meta/`. This matches how their write workloads scale (all
three KB-sized KV) and how their tail latency couples (all three
share the same fsync queue + LSM compaction reserve).

**Resolver consequence:** the resolver's per-tier preference list
(e.g. `Tag("fast-small") → Class(Nvme) → Class(Ssd) → DataDir`)
matches against `DeviceEntry` records discovered from `/proc/
mounts`. An LVM-backed mount appears as one `DeviceEntry` — the
resolver does not see (or care about) the underlying PVs. The
`DeviceEntry.media_class` is read from the slowest PV's
rotational flag (conservative — a mixed VG reports `Hdd` even
if it includes one SSD). Operator hygiene: don't mix media
classes inside a single VG.

**§D4.5 formula consequence:** `f_total` sums each node's fast-
tier mount capacity from `statvfs()`. LVM-aggregated mounts
report their total LV size — so a `vgextend + lvextend +
resize2fs` flow automatically grows `f_total` on next
`InventoryReporter` tick (60 s default). No formula change; no
catalog schema change; no apply-time re-budget required beyond
the existing inventory-refresh cadence.

### D9. Invariants

**I-DI1 (inventory freshness):** every node's inventory in the
catalog is no older than 90 s (60 s refresh interval + 30 s
tolerance for catalog apply delay).

**I-DI2 (resolver determinism):** given the same `(inventory,
policy)` snapshot, `resolve_path` MUST return the same path. The
algorithm is pure — no time, no random.

**I-DI3 (no fjall opens before policy):** runtime.rs MUST NOT open
any fjall store before the catalog policy is read. Boot-order
enforced by sequencing in `runtime::start`.

**I-DI4 (path subdirectory):** every fjall store path resolves to
`<device.mount_path>/kiseki/<tier_name>`. Operators rely on this
shape for `du`, backup, and migration scripts.

**I-DI5 (strict mode failure surfaces):** when policy mode is
Strict and the resolver can't find a matching device, the node
fails to start. Logs at `error!` level with the policy +
inventory + tier name so the operator can diagnose.

**I-DI6 (refresh idempotency):** a node re-publishing identical
inventory MUST be a no-op on the catalog (no `policy_revision`
bump, no `last_change_ms` update).

**I-DI7 (capacity formula determinism):** given the same
`(F_total, S_total, N_nodes, WorkloadParams)` snapshot, every
node MUST derive the same cluster-wide budgets via the §D4.5
formula. Per-node `budget_bytes` is then `cluster_<tier>_budget ×
(node.fast_capacity / F_total)`. No time, no random input. The
formula is re-runnable for `kiseki-admin topology capacity-policy
preview` against hypothetical inputs without touching cluster
state.

**I-DI8 (per-node budget sum):** for any node N with
`fast_capacity > 0`,
`N.metadata_share + N.small_file_share + N.headroom_share ≤
N.fast_capacity`. The resolver MUST reject any policy that
violates this on any node (boot-time failure in Strict mode,
warn-and-clamp in BestEffort). Protects against an operator
setting `Absolute` budgets that overcommit individual nodes
even when the cluster aggregate fits.

**I-DI9 (policy apply gate — F-4):** `SetPlacementPolicy` and
`SetWorkloadParams` apply on the control-plane state machine MUST
re-resolve budgets against the current catalog inventories.
Strict mode REJECTS the LogCommand at apply if any node would
violate I-DI8 under the new policy/workload (the LogCommand
returns `Err(PolicyOvercommit)` and the catalog state is
unchanged). BestEffort mode applies the LogCommand and emits a
structured `policy_apply_rebudget{node_id, tier, delta_bytes}`
event for every node whose budget shrinks below current actual
usage, plus a `kiseki_placement_path_mismatch{node, tier}` gauge.
Closes F-4's silent-overcommit risk: an operator pushing a
bad policy sees the rejection (Strict) or the rebudget event
stream (BestEffort) at apply time, not at next node restart.

**I-DI10 (canonical summation — auditor):** `F_total`, `S_total`,
and any cluster-aggregate sum derived from the catalog MUST be
computed by iterating `inventories: BTreeMap<NodeId, _>` in
ascending `NodeId` order. Float64 addition is non-associative;
this clause makes the formula bit-exact across nodes even though
floats are involved. Property tests in phase 3 confirm two
distinct compute orders agree on every input (I-DI7 + this
clause together = guaranteed determinism).

**I-CP-Move (path-move safety — F-3):** a fjall keyspace MUST
NOT be opened at a new resolved path while a non-empty keyspace
exists at the prior resolved path recorded in
`$KISEKI_DATA_DIR/kiseki-tier-paths.json`. See §D8.1 for the
algorithm. Required for ADR-040 I-CP1 close-to-open consistency
under policy change.

### D10. Observability

Prometheus gauges:

- `kiseki_device_inventory_entries{node_id, media_class}` — count
  of devices per class per node.
- `kiseki_device_inventory_free_bytes{node_id, mount_path}` —
  per-mount free bytes.
- `kiseki_placement_policy_revision` — current policy revision.
- `kiseki_fjall_store_path{node_id, tier}` (info) — the resolved
  path for each tier on each node.
- `kiseki_capacity_cluster_total_bytes{class}` — `F_total` and
  `S_total` per class.
- `kiseki_capacity_cluster_budget_bytes{tier}` — formula output
  per tier (Metadata, SmallObject, Headroom, Chunks).
- `kiseki_capacity_node_share_pct{node_id}` — this node's share
  of `F_total`, useful for "why does node 4 carry less" investigations.
- `kiseki_capacity_node_budget_bytes{node_id, tier}` — per-node
  resolved budget per tier.
- `kiseki_capacity_node_actual_bytes{node_id, tier}` — current
  on-disk usage per tier; alongside the budget gauge an operator
  sees drift.
- `kiseki_capacity_node_overcommit_bytes{node_id, tier}` — if
  `actual > budget`, the excess (Bytes); a non-zero value
  indicates §D8 migration needed or §D7 capacity-policy override.

Tracing: every `resolve_tier` call emits a `debug!` with the
inputs and chosen `(path, budget)`; every `UpsertNodeInventory`
apply emits an `info!` with the diff vs prior inventory; every
policy or workload update emits an `info!` with the formula
outputs at the new revision.

### D11. Scope (this ADR)

In scope:
- Per-node inventory model + discovery.
- Cluster catalog in control-plane Raft.
- Placement policy data model + default policy.
- §D4.5 capacity allocation formula (cluster-aggregate budgets
  with per-node proportional distribution).
- `WorkloadParams` data model + admin override surface.
- Resolver algorithm (path + budget).
- Boot-time integration for the five fjall tiers listed in D4.
- Admin RPC + CLI (placement policy, capacity policy, workload
  tuning, what-if preview).
- v1 operator-driven migration story.
- Invariants + observability (including formula determinism + per-node
  overcommit detection).

Out of scope (follow-ups):
- Automatic migration on policy change (v2).
- udev-listener-driven hardware-change detection.
- Per-tenant placement / capacity overrides (some tenants want
  their meta on separate devices or larger budgets).
- Per-shard placement (some shards on hotter media).
- `KISEKI_RAW_DEVICES` integration (chunk pool placement stays its
  own axis — different consumer model, different lifecycle).
- Cross-node automatic device-class rebalancing.
- Mid-run resolver re-runs (budgets stay fixed at boot; live
  rebalance lands with v2 migration).

### D11.1. Boundaries with related ADRs (rev 3)

The rev-3 architect/adversary review identified cross-ADR
contradictions. This section pins each boundary so the implementer
doesn't re-derive the wrong answer.

| ADR | Concern | Resolution |
|---|---|---|
| **ADR-016 (backup/DR)** | The catalog state machine is included in `ControlSnapshot` (serde_json), so backup catches it for free. **Bound**: snapshot size grows ~1 KiB per node. For clusters > 1000 nodes, switch the catalog encoding to postcard or per-tenant sharding (follow-up). | No code change in v1. |
| **ADR-022 (fjall keyspace pattern)** | Every catalog-resolved store uses the rev-2/3/4 fjall pattern (memtable + WAL + `PersistMode`). The §D2.5 RaftLog bootstrap path also uses fjall. | rev-5 of ADR-022 (small-object swap) consumes the resolver. Phase 5. |
| **ADR-024 (device management + capacity)** | Two type lattices for media: `MediaType` (system_disk) and `DeviceClass` (chunk pool). Q13 resolution: `MediaType` is the coarse owner; `DeviceClass` is a refinement tag on `DeviceEntry.device_class: Option<DeviceClass>`. Resolver only consults `MediaType`. Mapping table in Q13. | Phase 3 implementation refactors `DeviceClass` to carry an upcast to `MediaType`. |
| **ADR-029 (raw block allocator)** | Chunk fragments stay on `KISEKI_RAW_DEVICES` — raw block, no FS, no LVM. The fast-tier mounts that the resolver picks (`/mnt/fast-small`, `/mnt/fast-meta`) ride a parallel OS-layer LVM axis (§D8.2) that ADR-029 never sees: the script peels the trailing N NVMe off `${raw_devices}` for FS provisioning before the chunk store sees the list. ADR-049 §D4 chunk tier's `Remainder` capacity is **informational** (reports the unused fast leftover); actual chunk allocation is bounded by raw-device sizes, not by this number. | No change to ADR-029. The chunk axis and the fast-tier-FS axis are orthogonal. |
| **ADR-030 (small-file placement)** | `small_file_budget_bytes` was a per-shard input from `NodeMetadataCapacity` reporting (ADR-030 §3 / I-L9). ADR-049 §D4.5 now OWNS this number. ADR-030's I-L9 inline_threshold formula consumes the resolver output verbatim. ADR-030 needs a §"Rev 4 amendment" noting the source change. | ADR-030 amendment ships with ADR-049 phase 5 wiring. |
| **ADR-040 (persistent metadata stores)** | CompositionMeta path now resolver-routed. Without I-CP-Move (§D8.1), a path change between boots violates ADR-040 I-CP1. **Resolved by I-CP-Move + path-version pointer file.** | Phase 5 wires the pointer file before each fjall open. |
| **ADR-041 (Raft transport multiplexing)** | The control-plane group registers via the multiplexed listener (`registry_for_ctrl` at runtime.rs:783-794). Catalog reads/writes ride the existing control-plane plumbing. | No change. |
| **ADR-045 (tiered namespaces + per-class quotas)** | Q14 resolution: Inline-durability namespaces consume **RaftLog** budget, NOT SmallObject (the inline payload rides the Raft delta). ADR-045's per-tenant quota is unaffected. `WorkloadParams.inline_payload_factor` (default 0) lets operators model Inline pool overhead. | §D4.5 formula extended in phase 3 to honor `inline_payload_factor`. |
| **ADR-047 (decoupled-ack)** | IntentStore is one of the catalog-routed tiers. ADR-047's PART 8 dedup window + recent-incorporated set live in the per-shard state machine; their fjall path comes from the resolver in phase 5. | Phase 5 wires `OpenRaftLogStore::new(intent_store_path)` through the resolver. |
| **ADR-048 (slab-EC compactor)** | The slab-EC compactor reads `cluster_chunk_state` from the per-shard state machine. No fjall-store dependency. Orthogonal. | No interaction. |

### D12. Implementation phases

| Phase | What | Branch |
|---|---|---|
| 1 | Inventory + catalog data model. `ControlCommand` variants (`UpsertNodeInventory`, `SetPlacementPolicy`, `SetWorkloadParams`) + apply path in `ControlStateMachine`. `ControlSnapshot` field addition. JSON wire tests; round-trip a `ClusterDeviceCatalog` through `to_vec`/`from_slice` | `feat/049-1-catalog` |
| 2 | Per-node inventory discovery (`/proc/mounts` walk + `detect_media_type` + tag application) + boot publish + periodic refresh task (`InventoryReporter`). `KISEKI_DEVICE_TAGS` env-var parser. I-DI6 idempotency property test | `feat/049-2-discovery` |
| 3 | Placement policy + `WorkloadParams` data model + path resolver + §D4.5 capacity formula + per-node share derivation. Pure `compute_cluster_budgets` + `distribute_to_node` functions. Property tests: I-DI7 (determinism), I-DI8 (per-node budget sum), I-DI10 (canonical summation), §D4.5 edge-case table (all 8 rows), per-tier min-viable floor (Q10). **Measure `per_tier_min_viable_bytes` actual on existing fjall stores; revise default if ≠ 256 MiB.** | `feat/049-3-resolver` |
| 4 | Admin RPC + CLI + HTTP routes. Subcommands: `topology node-inventory list/show`, `topology placement-policy show/set/set-store-prefs`, `topology capacity-policy show/set-store/preview`, `topology workload show/set`. HTTP equivalents. I-DI9 policy-apply gate enforced in `ControlCommand::Set*` apply | `feat/049-4-admin` |
| 5 | Runtime wiring for the four catalog-resolved fjall tiers (SmallObject, IntentStore, CompositionMeta, ChunkMeta). Boot sequence reorder in `runtime.rs`: control-plane Raft (at bootstrap path per §D2.5) BEFORE catalog-resolved fjall stores. `kiseki-tier-paths.json` pointer file + I-CP-Move gate. ADR-022 rev-5 SmallObjectStore fjall swap consumes the resolver. ADR-030 amendment for I-L9 source change | `feat/049-5-runtime` |
| 6 | Migration command (`kiseki-admin storage migrate --tier=<name> --node=<id>`) with quiesce + rsync + pointer-file update + clear. BDD scenarios DI-1..DI-5: <br>**DI-1**: single-NVMe-node cluster — default policy resolves SmallObject to NVMe.<br>**DI-2**: heterogeneous cluster (one 200 GiB root NVMe node) — per-node share matches the §D4.5 worked example.<br>**DI-3**: Strict-mode policy with missing device class → node refuses to start, exits 1.<br>**DI-4**: placement-policy change + operator-driven migration (placement-only AND capacity-only AND combined cases). DI-4b: same change + non-quiesced reboot of one node MUST fail to start that node, other replicas serve.<br>**DI-5**: I-DI8 overcommit (Absolute SmallObject > F_total) rejected by `SetPlacementPolicy` apply (Strict) and emits rebudget events (BestEffort). | `feat/049-6-migration` |
| 7 | infra/gcp boot script: mount lssd at `/mnt/kiseki-fast-0..N`; set `KISEKI_DEVICE_TAGS` to flag them; set `KISEKI_RAFT_LOG_DIR` to the fastest lssd. Verify §D4.5 default formula yields the expected per-tier budgets on `c3-standard-22-lssd × 6`. **Phase 7 also measures `fast_headroom_pct` actual under sustained PUT load** (Q18) and proposes a default revision. | `feat/049-7-infra-gcp` |
| 8 | Adversary review + auditor sign-off (gate 2). Phase 3 measurements feed into a `WorkloadParams` defaults revision PR | (review) |

#129 resumes after phase 5 lands (the fjall swap is part of phase 5
because the swap can't ship before the resolver exists).

## Consequences

### Positive

- Heterogeneous-hardware deployments get correct placement without
  per-node operator hand-holding.
- Every fjall store (current + future) gets routed correctly by
  default — no per-store env-var bolt-ons.
- Cluster catalog gives ops a single place to see "what does each
  node have, what's the policy, what's actually being used."
- Sets up future per-tenant + per-shard placement extensions.

### Negative

- Boot path gets a Raft round-trip (UpsertNodeInventory + policy
  read). Adds ~5-50 ms to node startup on a healthy cluster.
- Control-plane state machine grows another responsibility. The
  catalog is small (one entry per node) but adds an apply path
  and a wire format to maintain.
- Migration story v1 is operator-driven; an inattentive ops team
  could leave a tier on the wrong device after a policy change.
  Mitigated by observability (D10 gauges show drift).
- Test surface area: 3 new BDD scenarios, ~20 new unit tests,
  cross-context integration tests for the boot ordering.

### Risk

- Boot-time dependency on control-plane Raft. If the control-plane
  group is wedged, no node can open its fjall stores → cluster
  can't start. Mitigation: cache the last known policy locally
  (`KISEKI_DATA_DIR/policy-cache.json`) and use it on boot when
  the catalog is unreachable. Catalog updates overwrite the cache.

## Open questions

(Rev 3 resolutions are marked **RESOLVED**; remaining items are
phase-3 acceptance criteria.)

1. **RESOLVED (rev 3 §D5)**: resolver memoizes at boot. Live
   policy changes propagate via migration (§D8) and the
   `policy_apply_rebudget` event stream (I-DI9), not by live
   re-resolution.

2. Should `KISEKI_DEVICE_TAGS` be the only operator-input surface,
   or also admin RPC (`topology node-inventory upsert --node=N
   --device=<json>`)? Env var is simpler for the boot script;
   admin RPC is needed for runtime updates without restart. Ship
   both — env var seeds initial state, admin RPC overrides.

3. How does the `KISEKI_RAW_DEVICES` chunk pool axis relate to
   this catalog? Both are device-axis-of-the-system but consumed
   by different layers. Keep them orthogonal in v1; future ADR
   may unify under a single "kiseki cluster sees these devices"
   model. Document the boundary in §D11 (out of scope) above.

4. What does the policy-yaml schema look like? Sketch:

   ```yaml
   tiers:
     - tier: SmallObject
       preferences: [{class: Nvme}, {class: Ssd}, DataDir]
       mode: BestEffort
     - tier: RaftLog
       preferences: [{tag: nvme-fast}, DataDir]
       mode: Strict
   ```

   Decide YAML vs JSON vs postcard during phase 1 implementation.

5. Adversary: what's the failure mode if two nodes have conflicting
   policy revisions during a rolling deploy? Solved by Raft
   serialization — policy_revision is monotone in the catalog.
   Document the bound: a node booting under policy_revision N
   may need to wait for the catalog to catch up to its current
   revision before resolving. Add a timeout + observability.

6. Capacity formula: is `avg_file_bytes = 256 KiB` the right
   default? It's a sweep between HPC (~10 MiB) and small-file
   (~64 KiB) workloads. Operator can tune via `topology workload
   set`, but the default determines first-boot behavior. Survey
   typical workload mixes on existing kiseki deploys (when there
   are any) and revise.

7. Capacity formula: does `growth_headroom = 1.5` need to scale
   with `S_total`? On a 1 PiB cluster, 1.5× of projected metadata
   is large in absolute bytes but a small share of F_total. On a
   100 TiB cluster, the same multiplier is a smaller absolute
   buffer. Consider `growth_headroom = max(1.5, log10(S_total
   in GiB) / 3)` — but lands as future tuning, not v1.

8. Should `node.fast_capacity` include the boot disk if it's
   NVMe-class? On `c3-standard-22-lssd`, the boot disk IS NVMe
   per `/sys/block/nvme0n1/queue/rotational`. Including it gives
   the §D4.5 formula more headroom; excluding it (via a "boot-only"
   tag) protects boot-disk free space for OS use. Default: include
   non-boot mounts only (operator tag the boot mount as
   `boot-only` via `KISEKI_DEVICE_TAGS=/=boot-only`); allow
   override via `--include-boot-disk` policy flag.

9. Adversary: an operator sets `Absolute { cluster_bytes: 100 TiB
   }` for SmallObject on a cluster with `F_total = 10 TiB`. What
   happens? Per I-DI8, the resolver rejects at boot (Strict) or
   clamps to `F_total - metadata - headroom` (BestEffort) and
   emits a `warn!`. Confirm the rejection path doesn't brick the
   cluster (Strict mode failure is restart-friendly — operator
   adjusts policy and reboots, no data loss).

10. **RESOLVED (rev 3 §D4.5 + WorkloadParams)**: per-node share
    truncation handled by `per_tier_min_viable_bytes` (default
    256 MiB, phase 3 measurement target). BestEffort redistributes
    the floor amount from largest-share nodes; Strict rejects.
    Property test in phase 3: 100-node × 1-GiB-smallest-share
    case clears the floor or fails the policy apply.

11. **RESOLVED (rev 3 §D2.5)**: RaftLog bootstrap-only via
    `KISEKI_RAFT_LOG_DIR` resolves the cold-boot deadlock.
    Phase 3 acceptance: a fresh single-node bootstrap and a
    fresh 3-node multi-node bootstrap both succeed without the
    catalog existing yet.

12. **RESOLVED (rev 3 §D8.1)**: path-move via I-CP-Move +
    `kiseki-tier-paths.json` pointer file. Phase 3 acceptance:
    a 3-node cluster with a placement-policy change AND a
    non-quiesced reboot of one node MUST fail to start that
    node (clear error) AND preserve hydrator consistency on the
    other two (ADR-040 I-CP1 still holds cluster-wide).

13. **RESOLVED (rev 3 §D11)**: `MediaType` (system_disk) vs
    `DeviceClass` (chunk pool) reconciled. ADR-049 owns the
    coarse `MediaType` lattice; ADR-024 `DeviceClass` is a
    refinement tag carried on `DeviceEntry.device_class:
    Option<DeviceClass>` for chunk-pool consumers. Resolver
    only consults `MediaType`. Mapping table:
      | MediaType | DeviceClass(es) |
      |---|---|
      | Nvme | NvmeU2, NvmeQlc (with WARN if QLC), NvmeSsd |
      | Ssd | SsdSata, SsdSas |
      | Hdd | HddEnterprise, HddArchive |
      | Unknown | Mixed, Custom |

14. **RESOLVED (rev 3 §D11)**: Inline-durability namespace
    (ADR-045 §D6) consumes **RaftLog** budget (the inline
    payload rides the Raft delta), NOT SmallObject budget.
    ADR-045's quota accounting (×1 multiplier) is unchanged.
    A new `WorkloadParams.inline_payload_factor` (default 0,
    i.e. no inline-durability traffic assumed) adjusts the
    RaftLog dimension when an operator enables Inline pools at
    scale. Out of scope for v1; spec'd in §D11 for adversary
    sign-off.

15. **RESOLVED (rev 3 F-6 / D5.5 `await_catalog_ready`)**:
    catalog quiescence gate prevents the concurrent-boot race.
    Each node waits for `last_change_ms` to age past
    `quiescence_ms` (default 2× refresh = 120 s) OR for an
    explicit `MarkClusterReady` admin command (cluster
    bootstrap shortcut).

16. **RESOLVED (rev 3 I-DI9)**: `SetPlacementPolicy` /
    `SetWorkloadParams` apply runs the I-DI8 gate against
    current inventories; Strict rejects, BestEffort emits
    rebudget events.

17. **RESOLVED (rev 3 §D4.5 floor)**: per-node minimum viable
    budget = `per_tier_min_viable_bytes` (default 256 MiB,
    phase 3 measurement re-tunes). Redistribution algorithm
    spec'd in §D4.5.

18. **DEFERRED to phase 3 measurement**: `fast_headroom_pct`
    default. Rev 3 bumped 10 → 25 based on LSM compaction
    literature; phase 3 instruments existing fjall stores
    (composition meta + chunk meta + intent store) under
    write pressure and re-tunes. Acceptance: actual measured
    worst-case ≤ 90% of `fast_headroom_pct × F_total`.

19. **DEFERRED to phase 6 implementation**:
    `growth_headroom` scaling with `S_total` (Q7 rev 2).
    Default 1.5 stays for v1; revisit after operators run
    the cluster for a quarter and report metadata-growth
    actuals.

20. **DEFERRED to follow-up ADR**: per-tenant placement
    overrides + per-shard placement (per §D11 out of scope).
    Documented here so adversary sees the bound.

21. **DEFERRED to phase 7 (infra/gcp)**: workload-param
    survey for default-tuning. The 256 KiB `avg_file_bytes`
    is a sweep; phase 7 measures GCP perf-cluster workload
    actuals and proposes a default revision.

### Re-diamond (rev 4) acceptance criteria

The rev-4 re-diamond produced 12 phase-3 acceptance items
(adversary N-1..N-12) + 5 architect SHOULD-FIX items + the
auditor's algorithm clarifications. All are implementation-detail
level — they MUST be addressed during phase implementation, not
in another spec revision.

22. **N-1 (KISEKI_RAFT_LOG_DIR fallback chain)**: mandatory
    env-var ordering — `KISEKI_RAFT_LOG_DIR > KISEKI_DATA_DIR/raft >
    FAIL_FAST("set one of these")`. No silent CWD fallback.
    Phase 1 acceptance.

23. **N-2 (pointer-file atomicity)**: `kiseki-tier-paths.json`
    written by `runtime.rs` (not per-tier consumer); atomic
    `write+rename` contract; `0600` permissions; corrupt JSON
    treated as `RefuseToOpen` (NOT first-boot); the file is
    written once after ALL tiers resolve. Phase 5a acceptance.

24. **N-3 (I-DI9 determinism note)**: code-comment + property
    test confirming `SetPlacementPolicy.apply` evaluates against
    the state-machine's current `inventories` BTreeMap, NOT an
    inventory snapshot embedded in the LogCommand. Add a
    leader-side pre-flight that gives operators a friendlier
    rejection at submit time, with the apply gate as safety net.
    Phase 4 acceptance.

25. **N-4 (F_total/S_total = 0 panic guards)**: explicit
    `S_total == 0` short-circuit (return zero-budgets struct,
    warn, refuse `SetPlacementPolicy` in Strict);
    explicit `floor > ceiling` handling (cap floor at ceiling,
    warn). Phase 3 acceptance.

26. **N-6 (QLC dimension surfacing)**: WARN fires at inventory
    publish (one log line per device, NOT silenced after first
    refresh); `kiseki_placement_qlc_inferred{node, mount}` gauge.
    Follow-up ADR splits `MediaType` for proper NvmeU2 vs NvmeQlc
    treatment. Phase 2 acceptance for the gauge.

27. **N-7 (per-node safety margin)**: apply a 0.95 multiplicative
    safety on per-node share OR on the I-DI8 RHS. Pick one and
    document. Phase 3 acceptance (formula or invariant text).

28. **N-8 (canonical-order summation lint)**: clippy/lint rule
    OR a property test that hashes the float sequence produced
    by two thread orders summing the same inventory; canonical-order
    bit-exact; randomized may differ. The phase 3 test commitment
    needs the wording fixed (rev 3 had it backwards). Phase 3
    acceptance.

29. **N-9 (boot publish-before-resolver-gate)**: boot order
    MUST be: (1) read local devices NOW; (2) publish
    `UpsertNodeInventory` with current truth; (3) THEN run
    resolver against the freshly-published catalog. Otherwise
    a recovering node with stale catalog truth bricks itself.
    Phase 5a acceptance.

30. **N-10 (25% headroom admin docs)**: `kiseki-admin topology
    node-inventory show --explain` prints the headroom rationale
    + formula inputs. `node-inventory show` (without --explain)
    labels the headroom share with an ADR-049 pointer.
    Phase 4 acceptance.

31. **N-11 (I-DI9 + I-CP-Move test commitments)**: phase 3
    pins unit tests for I-CP-Move covering: missing file →
    first-boot writes; corrupt JSON → refuses; matching path →
    pass-through; mismatched path with non-empty prior →
    refuses; mismatched path with empty prior → updates pointer.
    Phase 4 pins the I-DI9 apply-determinism property test.

32. **architect SHOULD-FIX (3 missing PlacementError variants)**:
    add `PolicyApplyRejected { reason, node_id }`,
    `PathVersionMismatch { tier, prior, resolved }`,
    `CatalogUnreachable { reason }`. Split `Io(io::Error)` into
    pointer-file I/O vs catalog-store I/O. Phase 3 acceptance.

33. **architect SHOULD-FIX (phase 5 split)**: split phase 5 →
    5a (boot reorder + I-CP-Move pointer file gate, single-node
    smoke test) + 5b (ADR-022 rev-5 SmallObjectStore fjall swap,
    #129 PUT works through resolver) + 5c (IntentStore +
    CompositionMeta + ChunkMeta consumer rewiring + ADR-030
    amendment, rolling-restart preserves all 4 tiers). Isolates
    the riskiest commit (5b touches data path).

34. **architect SHOULD-FIX (BDD feature file location +
    DI-6/DI-7)**: pre-allocate
    `specs/features/device-inventory.feature` (avoids collision
    with ADR-024 `device-management.feature`). Add DI-6
    (I-CP-Move dedicated: pointer-file deleted out of band)
    and DI-7 (catalog-quiescence timeout) scenarios. Phase 6.

35. **architect SHOULD-FIX (ADR-033/042/044 rows)**: extend
    §D11.1 with rows for ADR-033 (apply hook for I-DI9),
    ADR-042 (leader-forward for inventory upsert), ADR-044
    (policy-cache.json encryption at rest). Spec text addition.

36. **architect SHOULD-FIX (`#[serde(default)]`)**: phase 1
    contract requires `ControlSnapshot::catalog` field carries
    `#[serde(default)]` so a pre-upgrade snapshot decodes
    cleanly. Phase 1 acceptance.

37. **architect SHOULD-FIX (phase 7 measurement → phase 3)**:
    move `fast_headroom_pct` measurement commitment from phase 7
    to phase 3 alongside `per_tier_min_viable_bytes` measurement.
    Phase 7 just validates that production defaults produce
    sane budgets on `c3-standard-22-lssd × 6`. Spec text edit
    to §D12.

38. **auditor clarification (Absolute pre-check scope)**: the
    rev 3 cluster-aggregate Absolute pre-check catches pure
    Σ Absolute overcommit; mixed Auto+Absolute under-budget
    cases fall through to per-node I-DI8 post-distribution.
    Add this clarification to §D4.5. Phase 3 may add a stricter
    pre-check if mixed cases prove operationally noisy.

### Gate-1 verdict (post-rev-4)

Rev 4 addresses all MUST-FIX items from the re-diamond
verification. Remaining items (Q22-Q38) are phase-implementation
acceptance criteria — they pin the contracts the spec needs,
not structural redo. **Gate-1 PASS** (conditional on phase-3+
verification that Q22-Q38 land in code).
