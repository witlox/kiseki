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

- **rev 1** (2026-06-02, this revision): initial draft.

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
}

enum DeviceMatcher {
    Tag(String),
    Class(MediaType),
    DataDir,
}

enum PolicyMode { Strict, BestEffort }

struct PlacementPolicy {
    tiers: Vec<TierPolicy>,
}
```

Default policy (no operator config):

| Tier | Preferences (best-effort) |
|---|---|
| SmallObject | `Class(Nvme), Class(Ssd), DataDir` |
| IntentStore | `Class(Nvme), Class(Ssd), DataDir` |
| RaftLog | `Class(Nvme), Class(Ssd), DataDir` |
| CompositionMeta | `Class(Nvme), Class(Ssd), DataDir` |
| ChunkMeta | `Class(Nvme), Class(Ssd), DataDir` |

All metadata wants fast media; chunk fragments stay on
`KISEKI_RAW_DEVICES` (orthogonal). Best-effort by default: a node
with only HDD warns loudly but still serves.

Strict mode is for production fleets where any node missing the
declared tier should refuse to start (deployment regression catch).

### D5. Per-node path resolver

At boot, after the inventory is published and the policy is read
from the catalog, each node runs the resolver for every fjall tier:

```rust
fn resolve_path(
    inventory: &NodeDeviceInventory,
    policy: &TierPolicy,
    tier: FjallStoreTier,
) -> Result<PathBuf, PlacementError>;
```

Algorithm:

1. Walk `policy.preferences` left-to-right.
2. For each matcher, find the first `DeviceEntry` in `inventory`
   whose `media_class` / `tag` matches.
3. If found, the path is `entry.mount_path / "kiseki" / tier_name`.
4. If none of the preferences match: `Strict` returns
   `PlacementError::NoMatchingDevice`; `BestEffort` falls back to
   `data_dir / tier_name` and emits a warning.

Per-tier subdirectory under each device path: `"kiseki/<tier>"`,
e.g. `/mnt/nvme0/kiseki/small-object`. Predictable enough for
operators to `du -sh /mnt/nvme0/kiseki/*` and see per-tier usage.

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

New admin commands (extend `kiseki-admin topology`):

```
kiseki-admin topology node-inventory list
kiseki-admin topology node-inventory show --node=<id>
kiseki-admin topology placement-policy show
kiseki-admin topology placement-policy set --from-file=<yaml>
kiseki-admin topology placement-policy set-tier --tier=<name> \
    --prefer=<class-or-tag>,<class-or-tag>,... [--mode=strict|best-effort]
```

The HTTP surface mirrors:
- `GET  /admin/topology/devices` — full catalog
- `GET  /admin/topology/devices/<node-id>` — single node
- `GET  /admin/topology/placement-policy` — current policy
- `PUT  /admin/topology/placement-policy` — replace policy

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

### D10. Observability

Prometheus gauges:

- `kiseki_device_inventory_entries{node_id, media_class}` — count
  of devices per class per node.
- `kiseki_device_inventory_free_bytes{node_id, mount_path}` —
  per-mount free bytes.
- `kiseki_placement_policy_revision` — current policy revision.
- `kiseki_fjall_store_path{node_id, tier}` (info) — the resolved
  path for each tier on each node.

Tracing: every `resolve_path` call emits a `debug!` with the
inputs and chosen path; every `UpsertNodeInventory` apply emits an
`info!` with the diff vs prior inventory.

### D11. Scope (this ADR)

In scope:
- Per-node inventory model + discovery.
- Cluster catalog in control-plane Raft.
- Placement policy data model + default policy.
- Resolver algorithm.
- Boot-time integration for the five fjall tiers listed in D4.
- Admin RPC + CLI.
- v1 operator-driven migration story.
- Invariants + observability.

Out of scope (follow-ups):
- Automatic migration on policy change (v2).
- udev-listener-driven hardware-change detection.
- Per-tenant placement overrides (some tenants want their meta on
  separate devices).
- Per-shard placement (some shards on hotter media).
- `KISEKI_RAW_DEVICES` integration (chunk pool placement stays its
  own axis — different consumer model, different lifecycle).
- Cross-node automatic device-class rebalancing.

### D12. Implementation phases

| Phase | What | Branch |
|---|---|---|
| 1 | Inventory model, catalog state machine, control-plane LogCommand variants + apply, postcard wire tests | `feat/049-1-catalog` |
| 2 | Per-node inventory discovery + boot publish + periodic refresh task | `feat/049-2-discovery` |
| 3 | Placement policy data model + resolver algorithm + unit tests | `feat/049-3-resolver` |
| 4 | Admin RPC + CLI + HTTP routes | `feat/049-4-admin` |
| 5 | Runtime wiring for the five fjall tiers (boot sequence change in `runtime.rs`); ADR-022 rev-5 SmallObjectStore fjall swap consumes the resolver | `feat/049-5-runtime` |
| 6 | Migration command + BDD scenario for a 3-node cluster with policy change | `feat/049-6-migration` |
| 7 | infra/gcp boot script updates: mount lssd at predictable paths, set `KISEKI_DEVICE_TAGS` | `feat/049-7-infra-gcp` |
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
