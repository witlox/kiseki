//! Per-file metadata footprint + ADR-049 device-inventory and
//! placement-policy types.
//!
//! This module is the leaf-of-the-leaf: every dependent crate
//! (`kiseki-server`, `kiseki-chunk`, `kiseki-composition`, `kiseki-log`,
//! `kiseki-admin`) needs these types to drive ADR-049 cluster-side
//! device inventory + per-tier metadata placement + capacity allocation.
//! Keeping them in `kiseki-common` avoids a cycle: the catalog state
//! machine in `kiseki-server::cluster_control` mutates them; every fjall
//! consumer reads them at boot.
//!
//! ## Constant ownership
//!
//! [`PER_FILE_METADATA_FOOTPRINT_BYTES`] was historically at
//! `kiseki-server::system_disk::PER_FILE_METADATA_FOOTPRINT_BYTES`
//! (a binary-only crate). ADR-049 §D11.1 + Q22 relocate it here so
//! the constant is reachable from every consumer. The single source
//! of truth for the per-replica metadata footprint figure (512 B,
//! ADR-030 §1) lives in this module's doc comment on the constant.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::NodeId;

/// Conservative per-replica metadata footprint per file. Used by the
/// cluster aggregator to derive `cluster_max_files` from the
/// `Σ soft_limit_bytes` aggregated across nodes (ADR-030 §1) AND
/// by the ADR-049 §D4.5 capacity formula via
/// [`WorkloadParams::metadata_per_file_bytes`].
///
/// 512 bytes covers: composition record (~280 B), namespace hash
/// key reservation (32 B), name binding (~80 B avg), HLC stamps
/// (~16 B per dual-clock entry), and ~104 B reserved for fjall LSM
/// overhead and future fields. Aligns with the planning figure in
/// `docs/performance/capacity-planning.md`.
pub const PER_FILE_METADATA_FOOTPRINT_BYTES: u64 = 512;

/// Storage media type (ADR-049 §D1 + ADR-030 detection).
///
/// Reported by `kiseki-server::system_disk::detect_media_type` based
/// on Linux sysfs `rotational` flag + device-name prefix probe.
/// Returns `Unknown` on non-Linux or detection failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MediaType {
    /// `NVMe` SSD (non-rotational, nvme device).
    Nvme,
    /// SATA/SAS SSD (non-rotational).
    Ssd,
    /// Spinning disk (rotational).
    Hdd,
    /// Unknown (non-Linux or detection failed).
    Unknown,
}

impl MediaType {
    /// Label string for the prom info gauge. Stable for dashboards.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::Nvme => "nvme",
            Self::Ssd => "ssd",
            Self::Hdd => "hdd",
            Self::Unknown => "unknown",
        }
    }
}

/// One device entry in a node's inventory (ADR-049 §D1).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeviceEntry {
    /// Filesystem mount path, e.g. `/mnt/nvme0`.
    pub mount_path: PathBuf,
    /// Detected media class via `detect_media_type`.
    pub media_class: MediaType,
    /// Filesystem total bytes (`df`-style).
    pub total_bytes: u64,
    /// Free bytes at last refresh.
    pub free_bytes: u64,
    /// Operator-supplied tag from `KISEKI_DEVICE_TAGS`, e.g.
    /// `nvme-fast` or `boot-only`. Optional — enables policy
    /// targeting a specific tag rather than a class.
    pub tag: Option<String>,
    /// Whether the node has exclusive ownership of this mount
    /// (`true`) or shares it with other services (`false`).
    /// Affects fsync latency assumptions.
    pub exclusive: bool,
}

/// Per-node device inventory (ADR-049 §D1).
///
/// Published by every node via `ControlCommand::UpsertNodeInventory`
/// at boot + every `KISEKI_INVENTORY_REFRESH_MS` (default 60 s).
/// Stored in the cluster catalog
/// ([`ClusterDeviceCatalog::inventories`]) keyed by `NodeId`.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NodeDeviceInventory {
    /// The node this inventory belongs to.
    pub node_id: NodeId,
    /// Discovered + tagged devices on the node.
    pub devices: Vec<DeviceEntry>,
    /// Wall-clock at last refresh (epoch ms).
    pub refreshed_ms: u64,
}

impl NodeDeviceInventory {
    /// Sum of fast-tier (NVMe-class) capacity on this node, in
    /// bytes. Used as the per-node `fast_capacity` input to the
    /// ADR-049 §D4.5 capacity formula.
    #[must_use]
    pub fn fast_capacity(&self) -> u64 {
        self.devices
            .iter()
            .filter(|d| d.media_class == MediaType::Nvme)
            .map(|d| d.total_bytes)
            .sum()
    }

    /// Sum of slow-tier (SSD + HDD non-NVMe + Unknown) capacity on
    /// this node, in bytes. Used as the per-node `slow_capacity`
    /// input to the ADR-049 §D4.5 capacity formula. `Unknown`-class
    /// devices are treated as slow so the formula degrades safely
    /// when detection fails — the operator can override via
    /// `KISEKI_DEVICE_TAGS`.
    #[must_use]
    pub fn slow_capacity(&self) -> u64 {
        self.devices
            .iter()
            .filter(|d| !matches!(d.media_class, MediaType::Nvme))
            .map(|d| d.total_bytes)
            .sum()
    }
}

/// Which fjall store is being placed (ADR-049 §D4).
///
/// `RaftLog` is bootstrap-only per ADR-049 §D2.5 — its path comes
/// from `KISEKI_RAFT_LOG_DIR` (env var, fallback `KISEKI_DATA_DIR/raft`)
/// and is **never** resolver-routed. Listed here for cross-reference
/// so admin views can label it correctly.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub enum FjallStoreTier {
    /// Small-file inline content (`kiseki-chunk::SmallObjectStore`,
    /// becomes fjall under ADR-022 rev-5).
    SmallObject,
    /// ADR-047 intent store.
    IntentStore,
    /// openraft fjall log store (bootstrap-only, §D2.5).
    RaftLog,
    /// ADR-040 composition + namespace persistence (fjall).
    CompositionMeta,
    /// ADR-022 chunk + fragment metadata (fjall).
    ChunkMeta,
}

impl FjallStoreTier {
    /// Subdirectory name appended to the resolved device mount
    /// path. ADR-049 §D5 invariant I-DI4: every fjall store path
    /// resolves to `<device.mount_path>/kiseki/<tier_name>`.
    #[must_use]
    pub const fn dir_name(self) -> &'static str {
        match self {
            Self::SmallObject => "small-object",
            Self::IntentStore => "intent-store",
            Self::RaftLog => "raft-log",
            Self::CompositionMeta => "composition-meta",
            Self::ChunkMeta => "chunk-meta",
        }
    }

    /// Stable label string for Prom gauges / admin output.
    #[must_use]
    pub const fn as_label(self) -> &'static str {
        match self {
            Self::SmallObject => "small_object",
            Self::IntentStore => "intent_store",
            Self::RaftLog => "raft_log",
            Self::CompositionMeta => "composition_meta",
            Self::ChunkMeta => "chunk_meta",
        }
    }

    /// All four catalog-resolved tiers (excludes `RaftLog` which is
    /// bootstrap-only per §D2.5).
    #[must_use]
    pub const fn catalog_resolved() -> [Self; 4] {
        [
            Self::SmallObject,
            Self::IntentStore,
            Self::CompositionMeta,
            Self::ChunkMeta,
        ]
    }
}

/// A single matcher in the [`TierPolicy::preferences`] list
/// (ADR-049 §D4).
///
/// Resolver walks left-to-right; first matching `DeviceEntry`
/// wins. `Strict` mode requires exactly one of these to match;
/// `BestEffort` walks down the list until any matches and falls
/// back to `DataDir` otherwise.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum DeviceMatcher {
    /// Match a `DeviceEntry` whose `tag == Some(s)`.
    Tag(String),
    /// Match a `DeviceEntry` whose `media_class == c`.
    Class(MediaType),
    /// Match the `KISEKI_DATA_DIR` fallback (always present as the
    /// last-resort device entry, tagged `data-dir-default`).
    DataDir,
}

/// Strictness of the placement policy (ADR-049 §D4).
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PolicyMode {
    /// Refuses to resolve to any entry past the first matching
    /// preference; refuses to boot when none match. Production
    /// fleets use this for deployment-regression catch.
    Strict,
    /// Walks down the preference list until any matches; falls
    /// back to `DataDir` and warns when no preference matches.
    /// Default for fresh policies.
    BestEffort,
}

/// Per-tier placement decision (ADR-049 §D4).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TierPolicy {
    /// Which fjall store this policy applies to.
    pub tier: FjallStoreTier,
    /// Ordered preference list. Resolver walks left-to-right.
    pub preferences: Vec<DeviceMatcher>,
    /// `Strict` or `BestEffort`.
    pub mode: PolicyMode,
    /// Cluster-wide budget the resolver distributes to this node
    /// proportional to its local fast-tier share. See ADR-049
    /// §D4.5 for the formula. `RaftLog` MUST carry
    /// `TierCapacity::BootstrapOnly`.
    pub capacity: TierCapacity,
}

/// Cluster-wide capacity policy for a tier (ADR-049 §D4 + §D4.5).
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TierCapacity {
    /// Default formula: auto-derive cluster budget from
    /// [`WorkloadParams`] + per-tier clamps. Distributes per-node
    /// proportionally to local fast-tier capacity. See ADR-049
    /// §D4.5 pseudocode.
    Auto {
        /// Target fraction of cluster fast tier (`F_total`).
        /// For tiers tied to chunk capacity (`Metadata`) the
        /// formula derives the target from [`WorkloadParams`];
        /// this field then acts as the clamp ceiling.
        target_pct: u8,
        /// Cluster-wide minimum (sum over nodes). Common floor:
        /// `10 GiB × N_nodes` for Metadata so a brand-new cluster
        /// has working budget even before any files exist.
        floor_bytes: u64,
        /// Cluster-wide maximum. Optional. For `Metadata`: cap at
        /// `metadata_ceiling_pct_of_fast × F_total` to prevent
        /// over-projection from starving `SmallObject`.
        ceiling_bytes: Option<u64>,
    },
    /// Explicit absolute cluster-wide budget. Overrides `Auto`.
    /// Distributed per-node proportionally to local fast-tier
    /// share. ADR-049 I-DI8 + I-DI9 reject overcommitting absolutes.
    Absolute {
        /// Cluster-wide bytes to allocate to this tier.
        cluster_bytes: u64,
    },
    /// "Consume whatever's left after other tiers" — for Chunks.
    /// Resolver computes
    /// `node.chunk_budget = node.slow_capacity +
    /// (node.fast_capacity - Σ other_tier_budgets_on_this_node)`.
    Remainder,
    /// `RaftLog` only: never resolver-routed; path comes from
    /// `KISEKI_RAFT_LOG_DIR` (ADR-049 §D2.5 cold-boot deadlock
    /// fix). No budget tracked through the formula.
    BootstrapOnly,
}

/// Inputs to the ADR-049 §D4.5 capacity allocation formula.
///
/// Defaults shown on each field. Operator-overridable via
/// `kiseki-admin topology workload set`. Per Q22-Q38, phase 3
/// implementation MUST add `S_total == 0` short-circuit + canonical
/// summation order (I-DI10) + Q10 per-node-floor redistribution.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkloadParams {
    /// Average file size assumption. Default 256 KiB — sweeps
    /// between HPC (>1 MiB → tune down) and small-file workloads
    /// (<64 KiB → tune up). Drives
    /// `projected_files = S_total / avg_file_bytes`.
    pub avg_file_bytes: u64,
    /// Metadata replication factor. Default 3 (Replication-3).
    /// Composes [`Self::metadata_per_file_bytes`] with the
    /// per-replica footprint [`PER_FILE_METADATA_FOOTPRINT_BYTES`].
    pub metadata_replication: u8,
    /// Plan for `N×` metadata growth past projection. Default 1.5
    /// (×100, so `growth_headroom_x100 = 150`). f32 was rejected
    /// for Eq derive; store as `×100` integer.
    pub growth_headroom_x100: u16,
    /// Fraction of `F_total` always reserved for LSM compaction
    /// overhead. Default 25 (rev 3, was 10). Leveled compaction
    /// temporarily holds two copies of the level being merged —
    /// real worst-case is 30-50% for write-heavy workloads.
    pub fast_headroom_pct: u8,
    /// Maximum fraction of `F_total` Metadata can take. Default
    /// 30. Prevents projection over-estimate from starving
    /// `SmallObject`. Acts as `Auto`-mode ceiling for Metadata
    /// when the formula's natural output exceeds this.
    pub metadata_ceiling_pct_of_fast: u8,
    /// Per-tier minimum viable budget (per-node share floor) for
    /// fjall keyspace bring-up. Default 256 MiB — phase 3
    /// measures the actual keyspace overhead and re-tunes.
    /// Below this floor, fjall can't initialize a keyspace.
    pub per_tier_min_viable_bytes: u64,
}

impl Default for WorkloadParams {
    /// Defaults per ADR-049 rev 4 §D4.5 inputs.
    fn default() -> Self {
        Self {
            avg_file_bytes: 256 * 1024, // 256 KiB
            metadata_replication: 3,
            growth_headroom_x100: 150, // ×1.5
            fast_headroom_pct: 25,
            metadata_ceiling_pct_of_fast: 30,
            per_tier_min_viable_bytes: 256 * 1024 * 1024, // 256 MiB
        }
    }
}

impl WorkloadParams {
    /// Per-cluster metadata bytes per file. Composes the
    /// single-replica footprint
    /// ([`PER_FILE_METADATA_FOOTPRINT_BYTES`], 512 B per ADR-030
    /// §1) with [`Self::metadata_replication`].
    ///
    /// Default value: `3 × 512 B = 1536 B = 1.5 KiB`.
    #[must_use]
    pub fn metadata_per_file_bytes(&self) -> u64 {
        u64::from(self.metadata_replication) * PER_FILE_METADATA_FOOTPRINT_BYTES
    }

    /// `growth_headroom` as a float multiplier. Default 1.5.
    #[must_use]
    pub fn growth_headroom(&self) -> f64 {
        f64::from(self.growth_headroom_x100) / 100.0
    }
}

/// Cluster-wide placement policy (ADR-049 §D4).
///
/// Mutated through `ControlCommand::SetPlacementPolicy` apply.
/// Read by every node at boot to drive the resolver.
///
/// `Default` is the [`Self::built_in_default`] policy (tag-first then `Nvme` → `Ssd` →
/// `DataDir` preference per metadata tier) so a fresh cluster routes
/// fjall metadata to fast media without an explicit operator
/// `SetPlacementPolicy` apply. Custom is via
/// `kiseki-admin topology placement-policy set-store-prefs`.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PlacementPolicy {
    /// One [`TierPolicy`] per [`FjallStoreTier`]. The implementer
    /// MUST validate that exactly the four catalog-resolved tiers
    /// (`FjallStoreTier::catalog_resolved()`) appear, optionally
    /// plus `RaftLog` with `BootstrapOnly` capacity for
    /// observability. Missing tier → use built-in default.
    pub tiers: Vec<TierPolicy>,
}

impl Default for PlacementPolicy {
    fn default() -> Self {
        Self::built_in_default()
    }
}

impl PlacementPolicy {
    /// Built-in default policy per ADR-049 §D4 table.
    ///
    /// `SmallObject` prefers `Tag("fast-small")` first so operators
    /// can dedicate a small-files disk and avoid contention with the
    /// hot `IntentStore` latency path. The three Metadata-class tiers
    /// prefer `Tag("fast-meta")` first for the same reason. Both fall
    /// back through `Class(Nvme) → Class(Ssd) → DataDir` so single-
    /// `Nvme` / dev / CI deployments still pick a sensible disk
    /// without operator tagging.
    ///
    /// Capacity slots: `SmallObject` gets the bulk of the fast tier
    /// (80% target, 50 GiB × `N_nodes` floor); the three Metadata-
    /// class tiers share an `Auto{30% ceiling, 10 GiB × N floor}`
    /// slot via the §D4.5 formula's `metadata_budget` clamp.
    #[must_use]
    pub fn built_in_default() -> Self {
        let small_prefs = vec![
            DeviceMatcher::Tag("fast-small".to_string()),
            DeviceMatcher::Class(MediaType::Nvme),
            DeviceMatcher::Class(MediaType::Ssd),
            DeviceMatcher::DataDir,
        ];
        let meta_prefs = vec![
            DeviceMatcher::Tag("fast-meta".to_string()),
            DeviceMatcher::Class(MediaType::Nvme),
            DeviceMatcher::Class(MediaType::Ssd),
            DeviceMatcher::DataDir,
        ];
        let small_capacity = TierCapacity::Auto {
            target_pct: 80,
            floor_bytes: 50 * 1024 * 1024 * 1024, // 50 GiB per-node floor
            ceiling_bytes: None,
        };
        let metadata_capacity = TierCapacity::Auto {
            target_pct: 30,                       // ceiling for Metadata
            floor_bytes: 10 * 1024 * 1024 * 1024, // 10 GiB per-node floor
            ceiling_bytes: None,
        };
        Self {
            tiers: vec![
                TierPolicy {
                    tier: FjallStoreTier::SmallObject,
                    preferences: small_prefs,
                    mode: PolicyMode::BestEffort,
                    capacity: small_capacity,
                },
                TierPolicy {
                    tier: FjallStoreTier::IntentStore,
                    preferences: meta_prefs.clone(),
                    mode: PolicyMode::BestEffort,
                    capacity: metadata_capacity.clone(),
                },
                TierPolicy {
                    tier: FjallStoreTier::CompositionMeta,
                    preferences: meta_prefs.clone(),
                    mode: PolicyMode::BestEffort,
                    capacity: metadata_capacity.clone(),
                },
                TierPolicy {
                    tier: FjallStoreTier::ChunkMeta,
                    preferences: meta_prefs,
                    mode: PolicyMode::BestEffort,
                    capacity: metadata_capacity,
                },
            ],
        }
    }

    /// Find the [`TierPolicy`] for a given tier. Returns `None` if
    /// the policy doesn't list it (the resolver falls back to
    /// `built_in_default()` for that tier).
    #[must_use]
    pub fn for_tier(&self, tier: FjallStoreTier) -> Option<&TierPolicy> {
        self.tiers.iter().find(|t| t.tier == tier)
    }
}

/// The cluster-wide catalog (ADR-049 §D2 rev 3+).
///
/// Persisted in the control-plane state machine's
/// `ControlSnapshot::catalog` field. Two separate wall-clocks
/// per rev-4 N-5 fix: `policy_change_ms` is the
/// `await_catalog_ready` quiescence clock (mutated only by
/// policy / workload changes); `inventory_change_ms` is for
/// observability gauges (bumped on every inventory upsert,
/// which happens ~1.6×/sec on a busy 100-node cluster).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ClusterDeviceCatalog {
    /// One entry per node. Keyed for O(log N) upsert; iteration
    /// in ascending `NodeId` order is canonical (I-DI10).
    /// `BTreeMap` round-trips through `serde_json` with stable
    /// ordering — implementer MUST use `.iter()` (ordered), NOT
    /// `.values().par_iter()` or any randomized iterator.
    pub inventories: std::collections::BTreeMap<NodeId, NodeDeviceInventory>,
    /// Current placement policy. Mutated by `SetPlacementPolicy`.
    pub policy: PlacementPolicy,
    /// Current workload parameters. Mutated by `SetWorkloadParams`.
    pub workload: WorkloadParams,
    /// Monotone revision counter — bumped on `SetPlacementPolicy`
    /// AND `SetWorkloadParams` apply (NOT on `UpsertNodeInventory`).
    pub policy_revision: u64,
    /// Wall-clock (epoch ms) at last `SetPlacementPolicy` /
    /// `SetWorkloadParams` apply. THE `await_catalog_ready`
    /// quiescence clock. Inventory upserts do NOT touch this
    /// (rev-4 N-5 fix).
    pub policy_change_ms: u64,
    /// Wall-clock (epoch ms) at last `UpsertNodeInventory`
    /// apply. For D10 observability gauges only — NOT consulted
    /// by quiescence.
    pub inventory_change_ms: u64,
}

impl ClusterDeviceCatalog {
    /// Sum of every node's fast-tier capacity, in canonical
    /// `NodeId`-ascending order (I-DI10). Used as `F_total`
    /// input to the §D4.5 formula.
    #[must_use]
    pub fn f_total(&self) -> u64 {
        // BTreeMap::iter() yields in key order — canonical per
        // I-DI10. Bit-exact across nodes given identical state.
        self.inventories
            .values()
            .map(NodeDeviceInventory::fast_capacity)
            .sum()
    }

    /// Sum of every node's slow-tier capacity, in canonical
    /// `NodeId`-ascending order (I-DI10). Used as `S_total`
    /// input to the §D4.5 formula.
    #[must_use]
    pub fn s_total(&self) -> u64 {
        self.inventories
            .values()
            .map(NodeDeviceInventory::slow_capacity)
            .sum()
    }

    /// Count of nodes with `fast_capacity > 0`. Used as
    /// `N_nodes_with_fast` input to the §D4.5 formula.
    #[must_use]
    pub fn n_nodes_with_fast(&self) -> u64 {
        self.inventories
            .values()
            .filter(|inv| inv.fast_capacity() > 0)
            .count() as u64
    }

    /// Count of all nodes in the catalog (regardless of fast tier).
    /// Used as `N_nodes_total` input to the §D4.5 formula.
    #[must_use]
    pub fn n_nodes_total(&self) -> u64 {
        self.inventories.len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_workload_params_compose_metadata_bytes() {
        let wp = WorkloadParams::default();
        assert_eq!(wp.metadata_per_file_bytes(), 3 * 512);
        assert_eq!(wp.metadata_per_file_bytes(), 1536);
        assert!((wp.growth_headroom() - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn per_file_constant_matches_adr030_figure() {
        // ADR-030 / docs/performance/capacity-planning.md pin
        // this at 512 B. If this assertion ever changes,
        // update both docs in lock-step.
        assert_eq!(PER_FILE_METADATA_FOOTPRINT_BYTES, 512);
    }

    #[test]
    fn fjall_store_tier_catalog_resolved_excludes_raft_log() {
        let resolved = FjallStoreTier::catalog_resolved();
        assert_eq!(resolved.len(), 4);
        assert!(!resolved.contains(&FjallStoreTier::RaftLog));
        assert!(resolved.contains(&FjallStoreTier::SmallObject));
        assert!(resolved.contains(&FjallStoreTier::IntentStore));
        assert!(resolved.contains(&FjallStoreTier::CompositionMeta));
        assert!(resolved.contains(&FjallStoreTier::ChunkMeta));
    }

    #[test]
    fn media_type_round_trips_through_json() {
        for mt in [
            MediaType::Nvme,
            MediaType::Ssd,
            MediaType::Hdd,
            MediaType::Unknown,
        ] {
            let s = serde_json::to_string(&mt).expect("encode");
            let back: MediaType = serde_json::from_str(&s).expect("decode");
            assert_eq!(mt, back);
        }
    }

    #[test]
    fn cluster_catalog_canonical_summation() {
        // I-DI10: two iteration orders MUST agree on F_total.
        // BTreeMap guarantees ascending-key order regardless of
        // insertion order.
        let mut catalog_a = ClusterDeviceCatalog::default();
        let mut catalog_b = ClusterDeviceCatalog::default();

        let mk = |node_id_v: u64, fast_gib: u64, slow_gib: u64| {
            let gib = 1024 * 1024 * 1024;
            NodeDeviceInventory {
                node_id: NodeId(node_id_v),
                devices: vec![
                    DeviceEntry {
                        mount_path: PathBuf::from(format!("/mnt/nvme{node_id_v}")),
                        media_class: MediaType::Nvme,
                        total_bytes: fast_gib * gib,
                        free_bytes: fast_gib * gib,
                        tag: None,
                        exclusive: true,
                    },
                    DeviceEntry {
                        mount_path: PathBuf::from(format!("/mnt/sata{node_id_v}")),
                        media_class: MediaType::Ssd,
                        total_bytes: slow_gib * gib,
                        free_bytes: slow_gib * gib,
                        tag: None,
                        exclusive: true,
                    },
                ],
                refreshed_ms: 1_700_000_000_000,
            }
        };

        // Insert into catalog_a in ascending order, catalog_b
        // in descending order — both BTreeMaps iterate ascending.
        for n in [1u64, 2, 3, 4, 5, 6] {
            catalog_a.inventories.insert(NodeId(n), mk(n, 1500, 8000));
        }
        for n in [6u64, 5, 4, 3, 2, 1] {
            catalog_b.inventories.insert(NodeId(n), mk(n, 1500, 8000));
        }

        // Insertion order does not affect iteration order.
        assert_eq!(catalog_a.f_total(), catalog_b.f_total());
        assert_eq!(catalog_a.s_total(), catalog_b.s_total());
        // Worked-example expectation: 6 × 1.5 TiB = 9 TiB = 9216 GiB
        let gib = 1024 * 1024 * 1024;
        assert_eq!(catalog_a.f_total(), 6 * 1500 * gib);
        assert_eq!(catalog_a.s_total(), 6 * 8000 * gib);
        assert_eq!(catalog_a.n_nodes_with_fast(), 6);
    }

    #[test]
    fn heterogeneous_cluster_f_total_matches_adr049_worked_example() {
        // ADR-049 §D4.5 heterogeneous example: 5 lssd-class nodes
        // (1500 GiB NVMe each) + 1 root-NVMe node (200 GiB).
        // F_total = 200 + 5 × 1500 = 7700 GiB (rev 4 corrected
        // — rev 3 had 7880 which was wrong).
        let gib = 1024 * 1024 * 1024;
        let mut catalog = ClusterDeviceCatalog::default();
        for n in [1u64, 2, 3, 5, 6] {
            catalog.inventories.insert(
                NodeId(n),
                NodeDeviceInventory {
                    node_id: NodeId(n),
                    devices: vec![DeviceEntry {
                        mount_path: PathBuf::from(format!("/mnt/nvme{n}")),
                        media_class: MediaType::Nvme,
                        total_bytes: 1500 * gib,
                        free_bytes: 1500 * gib,
                        tag: None,
                        exclusive: true,
                    }],
                    refreshed_ms: 0,
                },
            );
        }
        // Node 4 has only 200 GiB root NVMe.
        catalog.inventories.insert(
            NodeId(4),
            NodeDeviceInventory {
                node_id: NodeId(4),
                devices: vec![DeviceEntry {
                    mount_path: PathBuf::from("/"),
                    media_class: MediaType::Nvme,
                    total_bytes: 200 * gib,
                    free_bytes: 200 * gib,
                    tag: Some("boot-only".into()),
                    exclusive: false,
                }],
                refreshed_ms: 0,
            },
        );

        assert_eq!(catalog.f_total(), 7700 * gib);
        assert_eq!(catalog.n_nodes_with_fast(), 6);
        assert_eq!(catalog.n_nodes_total(), 6);
    }

    #[test]
    fn placement_policy_built_in_default_covers_all_resolved_tiers() {
        let pol = PlacementPolicy::built_in_default();
        for tier in FjallStoreTier::catalog_resolved() {
            assert!(
                pol.for_tier(tier).is_some(),
                "built-in default must cover {tier:?}",
            );
        }
        // RaftLog is bootstrap-only and NOT in the default policy.
        assert!(pol.for_tier(FjallStoreTier::RaftLog).is_none());
    }

    #[test]
    fn catalog_round_trips_through_json_with_field_addition_tolerance() {
        // ADR-049 phase 1 acceptance Q36: `#[serde(default)]` on
        // future field additions means pre-upgrade snapshots
        // decode cleanly. We test the converse here: encode a
        // fully-populated catalog, decode it, expect equality.
        let mut catalog = ClusterDeviceCatalog {
            workload: WorkloadParams::default(),
            policy: PlacementPolicy::built_in_default(),
            policy_revision: 42,
            policy_change_ms: 1_700_000_000_000,
            inventory_change_ms: 1_700_000_001_000,
            ..ClusterDeviceCatalog::default()
        };
        catalog.inventories.insert(
            NodeId(7),
            NodeDeviceInventory {
                node_id: NodeId(7),
                devices: vec![DeviceEntry {
                    mount_path: PathBuf::from("/mnt/nvme0"),
                    media_class: MediaType::Nvme,
                    total_bytes: 1_000_000_000,
                    free_bytes: 999_000_000,
                    tag: Some("nvme-fast".into()),
                    exclusive: true,
                }],
                refreshed_ms: 1_700_000_000_500,
            },
        );

        let json = serde_json::to_vec(&catalog).expect("encode");
        let back: ClusterDeviceCatalog = serde_json::from_slice(&json).expect("decode");
        assert_eq!(catalog, back);
    }
}
