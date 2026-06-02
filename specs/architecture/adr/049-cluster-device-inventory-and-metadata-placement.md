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

- **rev 1** (2026-06-02): initial draft.
- **rev 2** (2026-06-02, this revision): adds §D4.5 capacity
  allocation formula (cluster-aggregate, not per-node), extends D5
  to produce per-tier budgets, extends D7 admin surface with
  capacity-policy commands. Driven by 2026-06-02 design discussion
  — heterogeneous-node clusters (NVMe-root-only mixed with
  NVMe-full) need cluster-wide budget targets that distribute
  proportionally, not flat per-node percentages; metadata budget
  ties to chunk-tier capacity (file count grows with bulk storage),
  not to fast-tier capacity. Adds I-DI7 (capacity formula
  determinism).

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

### D2. Cluster catalog

Inventories live in the **control-plane Raft group** (existing
ADR-041 multiplexed transport, ADR-033 control-plane group). State:

```rust
struct ClusterDeviceCatalog {
    inventories: BTreeMap<NodeId, NodeDeviceInventory>,
    policy: PlacementPolicy,
    policy_revision: u64,
    last_change_ms: u64,
}
```

Why control-plane Raft (not per-shard Raft, not gossip):
- **Per-shard** is wrong: device inventory is node-scoped, not
  shard-scoped. Multiple shards on one node share the same devices.
- **Gossip** loses idempotency: a node restart that re-publishes
  inventory needs the catalog to converge deterministically across
  the cluster. Raft gives that for free.
- **Control-plane** is the right scope: this is global cluster
  policy, the same surface as namespace topology and shard
  membership.

Mutations: two new `LogCommand` variants on the control-plane
state machine (after ADR-033's existing `RecordSplit` and friends —
new variants appended to keep postcard discriminant stability):

```rust
LogCommand::UpsertNodeInventory {
    node_id: NodeId,
    inventory: NodeDeviceInventory,
}
LogCommand::SetPlacementPolicy {
    policy: PlacementPolicy,
}
```

Reads are local (every node has the catalog in its state machine
inner). No new RPC for reads — admin CLI hits the existing
`/admin/topology/...` HTTP surface.

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
    /// Per-file metadata footprint (composition + name binding +
    /// dual-clock stamps + LSM overhead). Default 1.5 KiB =
    /// `PER_FILE_METADATA_FOOTPRINT_BYTES × R_metadata` with R=3.
    metadata_per_file_bytes: u64,
    /// Plan for N× metadata growth past projection. Default 1.5.
    /// Bigger value → reserves more fast tier for future files.
    growth_headroom: f32,
    /// Fraction of `F_total` always reserved for LSM compaction
    /// overhead. Default 10. Without it, fsync stalls under write
    /// pressure when memtable flushes hit level merge.
    fast_headroom_pct: u8,
    /// Maximum fraction of `F_total` Metadata can take. Default
    /// 30. Prevents projection over-estimate from starving
    /// SmallObject. Acts as Auto-mode ceiling for Metadata when
    /// the formula's natural output exceeds this.
    metadata_ceiling_pct_of_fast: u8,
}
```

Default policy (no operator config). All metadata tiers
collectively form the "Metadata" capacity slot — they share one
`Auto` capacity entry rather than four; see §D4.5 for why.
Chunks consume the remainder per `TierCapacity::Remainder`.

| Tier | Preferences (best-effort) | Capacity |
|---|---|---|
| SmallObject | `Class(Nvme), Class(Ssd), DataDir` | `Auto{ pct: 80, floor: 50 GiB × N }` |
| IntentStore | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| RaftLog | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| CompositionMeta | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| ChunkMeta | `Class(Nvme), Class(Ssd), DataDir` | (shares Metadata slot) |
| Metadata (synthetic — sum of IntentStore + RaftLog + CompositionMeta + ChunkMeta) | (resolves through member tiers) | `Auto{ pct: 30 (ceiling), floor: 10 GiB × N }` |
| (LSM headroom — not a real tier; reserved unallocated) | (on whichever class above lands) | `fast_headroom_pct = 10` |
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
inventory:

```
F_total = Σ node.fast_capacity         # NVMe-class mount totals
S_total = Σ node.slow_capacity         # SSD + HDD non-NVMe totals
N_nodes = | nodes with fast_capacity > 0 |
```

From `PlacementPolicy.workload` (defaults shown):

```
avg_file_bytes               = 256 KiB
metadata_per_file_bytes      = 1.5 KiB     # PER_FILE_METADATA_FOOTPRINT_BYTES × R=3
growth_headroom              = 1.5
fast_headroom_pct            = 10
metadata_ceiling_pct_of_fast = 30
```

#### Cluster-wide budgets

```
# Projected file count from chunk capacity, NOT fast capacity —
# metadata grows with the bulk store, not with the fast tier.
projected_files       = S_total / avg_file_bytes

# Cluster-wide metadata bytes needed, with growth headroom.
projected_metadata    = projected_files × metadata_per_file_bytes

# Clamp to floor (cold-start floor) and ceiling (don't starve
# SmallObject).
metadata_budget       = clamp(
                          projected_metadata × growth_headroom,
                          floor   = 10 GiB × N_nodes,
                          ceiling = (metadata_ceiling_pct_of_fast / 100) × F_total
                        )

# LSM compaction reserve — always 10% of F_total by default.
headroom_budget       = (fast_headroom_pct / 100) × F_total

# Small file gets the remainder of the fast tier.
small_file_budget     = max(0, F_total − metadata_budget − headroom_budget)

# Chunks consume slow tier + any unused fast tier.
chunk_budget          = S_total + max(0, F_total − metadata_budget
                                          − headroom_budget
                                          − small_file_budget)
                      # the last clause is always 0 by construction
                      # but kept explicit for the auditor
```

#### Per-node distribution

```
for each node n:
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
```

A node with zero fast tier hosts no metadata or small-file budget
— other nodes carry its share. Cluster aggregate budgets stay
satisfied.

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
(counted as slow). Defaults applied.

```
F_total            = 6 × 1.5 TiB = 9 TiB
S_total            = 6 × 8 TiB   = 48 TiB
N_nodes            = 6
projected_files    = 48 TiB / 256 KiB = 200 M
projected_metadata = 200 M × 1.5 KiB  = 286 GiB
metadata_budget    = clamp(429 GiB, 60 GiB, 2.7 TiB) = 429 GiB    (~4.6% of F_total)
headroom_budget    = 0.10 × 9 TiB      = 900 GiB
small_file_budget  = 9 TiB − 429 − 900 = 7.7 TiB                  (~85% of F_total)
chunk_budget       = 48 TiB on SATA + 0 fast spillover

per node (share = 1/6 each):
  metadata_share   = 429 / 6  = 71.5 GiB
  small_file_share = 7.7 / 6  = 1.28 TiB
  headroom_share   = 900 / 6  = 150 GiB
  chunk_share      = 8 TiB SATA per node
```

#### Worked example — heterogeneous, one root-NVMe node

Same cluster, but **node 4 has only 200 GiB root NVMe** (no lssd).

```
F_total = 200 GiB + 5 × 1.5 TiB = 7.7 TiB
N_nodes = 6                    # node 4 still counts; it has F > 0

projected_files    = 48 TiB / 256 KiB = 200 M
projected_metadata = 200 M × 1.5 KiB  = 286 GiB
metadata_budget    = clamp(429 GiB, 60 GiB, 2.31 TiB) = 429 GiB   (same)
headroom_budget    = 0.10 × 7.7 TiB    = 770 GiB
small_file_budget  = 7.7 TiB − 429 − 770 = 6.5 TiB                (~84% of F_total)

shares:
  node 4 share     = 200 GiB / 7.7 TiB = 2.5%
  nodes 1-3, 5-6   = 1500 GiB / 7.7 TiB = 19.5% each

  node 4 metadata  = 429 × 2.5%  = ~11 GiB
  node 4 small     = 6500 × 2.5% = ~163 GiB
  node 4 headroom  = 770  × 2.5% = ~19 GiB
                                            sum ~193 GiB → fits 200 GiB NVMe

  node 1 metadata  = 429 × 19.5% = ~84 GiB
  node 1 small     = 6500 × 19.5% = ~1.27 TiB
  node 1 headroom  = 770  × 19.5% = ~150 GiB
```

Node 4 still participates with proportionally smaller shares. The
cluster aggregate budgets are still satisfied. Cluster perf
doesn't collapse on the small-NVMe node — its metadata + small
combined fit within its 200 GiB.

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

When the policy changes, existing fjall store contents must move.
v1 approach (operator-driven):

1. Operator sets new policy via admin RPC.
2. Each affected node logs: "policy says SmallObject should live on
   `/mnt/nvme1` but I'm currently on `/mnt/sata0` — drain via:
   `kiseki-admin storage migrate --tier=small-object --node=N`".
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

### D12. Implementation phases

| Phase | What | Branch |
|---|---|---|
| 1 | Inventory model, catalog state machine, control-plane LogCommand variants + apply, postcard wire tests | `feat/049-1-catalog` |
| 2 | Per-node inventory discovery + boot publish + periodic refresh task | `feat/049-2-discovery` |
| 3 | Placement policy + `WorkloadParams` data model + path resolver + §D4.5 capacity formula + per-node share derivation + property tests (formula determinism I-DI7, overcommit guard I-DI8, edge-case table from §D4.5) | `feat/049-3-resolver` |
| 4 | Admin RPC + CLI + HTTP routes (placement, capacity, workload, what-if preview) | `feat/049-4-admin` |
| 5 | Runtime wiring for the five fjall tiers (boot sequence change in `runtime.rs`); ADR-022 rev-5 SmallObjectStore fjall swap consumes the resolver | `feat/049-5-runtime` |
| 6 | Migration command + BDD scenario for a 3-node cluster with policy change (placement AND capacity policy change cases) | `feat/049-6-migration` |
| 7 | infra/gcp boot script updates: mount lssd at predictable paths, set `KISEKI_DEVICE_TAGS`; verify the §D4.5 default formula yields the expected per-tier budgets on `c3-standard-22-lssd × 6` | `feat/049-7-infra-gcp` |
| 8 | Adversary review + auditor sign-off | (review) |

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

1. Should the resolver memoize at boot or refresh on every fjall
   open? Memoize at boot is simpler and matches the "boot-time
   topology" model. Refresh-on-open allows live policy changes to
   take effect mid-run for stores opened lazily. Prefer **memoize
   at boot**; migration handles policy changes.

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

10. Adversary: per-node `share` truncation. With 100 nodes and
    `F_total = 9 TiB`, the smallest `node.fast_capacity` might be
    1 GiB → share = 0.011%. After multiplying by
    `metadata_budget = 429 GiB`, this node gets ~5 MiB metadata.
    Below any reasonable per-store-fjall floor (~100 MiB for the
    keyspace metadata alone). Should the formula impose a
    per-node floor (e.g. `max(formula_share, 100 MiB)`)? Risk:
    cluster aggregate becomes over-budget. Mitigation: if any
    node hits the per-node floor, the floor amount is
    redistributed by removing it from the largest-share nodes'
    allocations. Decide during phase 3 implementation.
