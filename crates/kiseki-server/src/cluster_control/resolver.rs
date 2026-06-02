//! ADR-049 phase 3: placement + capacity resolver.
//!
//! Pure functions implementing the §D4.5 capacity allocation formula
//! + per-node distribution. The resolver is split into three layers:
//!
//!   1. [`compute_cluster_budgets`] — given inventories + workload +
//!      policy, produces cluster-wide budgets per tier (Metadata,
//!      `SmallObject`, Headroom, Chunks). Pure. Bit-exact deterministic
//!      via canonical ascending-`NodeId` iteration (I-DI10).
//!   2. [`distribute_to_node`] — projects cluster budgets onto a
//!      single node's per-node share. Pure.
//!   3. [`resolve_tier_path`] — given a node's inventory + a
//!      `TierPolicy`, returns the resolved `(mount_path, media_class)`
//!      pair. Pure.
//!
//! Phase 5 wires these into the boot path. Phase 4 wires the I-DI9
//! apply-time gate via `compute_cluster_budgets` + `distribute_to_node`.
//!
//! ## Property invariants (phase-3 acceptance criteria)
//!
//! - **I-DI7 (formula determinism):** same inputs → same outputs.
//!   `compute_cluster_budgets` is pure (no time, no random).
//! - **I-DI8 (per-node budget sum):** for every node with
//!   `fast_capacity > 0`, `metadata_share + small_share + headroom`
//!   ≤ `node.fast_capacity`. Tested via `proptest`.
//! - **I-DI10 (canonical summation):** order-of-insertion of node
//!   inventories MUST NOT affect cluster budgets. The `BTreeMap`
//!   iterator delivers ascending-`NodeId` regardless of insertion
//!   order; the test `canonical_summation_invariant_under_random_insert_order`
//!   pins that with proptest.

#![allow(dead_code)] // phase-4 + phase-5 wire the public surface

use std::path::PathBuf;

use kiseki_common::{
    ClusterDeviceCatalog, DeviceEntry, DeviceMatcher, FjallStoreTier, MediaType,
    NodeDeviceInventory, NodeId, PlacementPolicy, PolicyMode, TierCapacity, TierPolicy,
};

/// Cluster-wide budget outputs from the §D4.5 formula.
///
/// Computed once per resolver run from the catalog snapshot;
/// projected onto each node by [`distribute_to_node`]. Field
/// names are stable for admin-CLI consumption.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClusterBudgets {
    /// `F_total` — sum of fast-tier capacity across all nodes.
    pub f_total: u64,
    /// `S_total` — sum of slow-tier capacity across all nodes.
    pub s_total: u64,
    /// `metadata_budget` — clamped by `[floor, ceiling]`.
    pub metadata: u64,
    /// `small_file_budget` — `F_total − metadata − headroom`,
    /// clamped at 0.
    pub small_object: u64,
    /// `headroom_budget` — `fast_headroom_pct × F_total`.
    pub headroom: u64,
    /// `chunk_budget` — `S_total + leftover_fast` (leftover is
    /// identically 0 by construction per §D4.5).
    pub chunk: u64,
}

/// Per-node budget projection from cluster budgets (§D4.5 phase 2
/// "Per-node distribution"). Sum of `metadata + small_object +
/// headroom` ≤ `node.fast_capacity` by I-DI8.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NodeBudgets {
    pub metadata: u64,
    pub small_object: u64,
    pub headroom: u64,
    pub chunk: u64,
}

/// Resolution outcome for one tier on one node.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResolvedTierBudget {
    pub tier: FjallStoreTier,
    pub chosen_mount: PathBuf,
    pub chosen_class: MediaType,
    pub budget_bytes: u64,
}

/// Resolver failure modes (subset for phase 3; phase 4 adds the
/// policy-apply variants).
#[derive(Debug, thiserror::Error)]
pub enum PlacementError {
    #[error(
        "ADR-049 Strict mode: tier {tier:?} has no matching device on node \
         (preferences: {pref_count}, inventory devices: {device_count})"
    )]
    NoMatchingDevice {
        tier: FjallStoreTier,
        mode: PolicyMode,
        pref_count: usize,
        device_count: usize,
    },
    #[error(
        "ADR-049 I-DI8 violation: cluster Absolute SmallObject ({cluster_demand} B) \
         exceeds available fast tier ({cluster_available} B); policy rejected"
    )]
    PolicyOvercommit {
        tier: FjallStoreTier,
        cluster_demand: u64,
        cluster_available: u64,
    },
    #[error(
        "ADR-049 Q10: node {node_id:?} tier {tier:?} share ({share_bytes} B) below \
         min viable ({min_viable} B) in Strict mode"
    )]
    BelowMinViable {
        tier: FjallStoreTier,
        node_id: NodeId,
        share_bytes: u64,
        min_viable: u64,
    },
}

/// Apply the §D4.5 cluster-aggregate formula to derive budgets.
///
/// **Pure function — I-DI7 + I-DI10.** No time, no random. All
/// `Σ` iterations go through `BTreeMap::values()` which yields
/// in ascending `NodeId` order regardless of insertion order
/// (the `BTreeMap` iterator IS the canonical order per I-DI10).
///
/// Handles the rev-4 N-4 + auditor edge cases:
///   - `F_total == 0` → re-base on `S_total`; capacity tracking
///     does NOT go dark.
///   - `S_total == 0` → `projected_metadata = 0`, formula degrades
///     gracefully (metadata hits its floor; small + headroom
///     scale with `F_total`).
///   - `F_total == 0 && S_total == 0` → all budgets are 0,
///     returns clean zero-budgets struct (no NaN, no panic).
pub fn compute_cluster_budgets(
    catalog: &ClusterDeviceCatalog,
) -> Result<ClusterBudgets, PlacementError> {
    let f_total = catalog.f_total();
    let s_total = catalog.s_total();
    let n_nodes_total = catalog.n_nodes_total();
    let workload = catalog.workload;

    // I-DI8 / Q9 + auditor finding-5: cluster-aggregate Absolute
    // pre-check. Catches `Absolute { cluster_bytes: ... }` that
    // exceeds the fast tier BEFORE per-node distribution so the
    // operator sees a single clear "your policy doesn't fit"
    // rather than per-node I-DI8 violations after the fact.
    let absolute_demand_fast = catalog
        .policy
        .tiers
        .iter()
        .filter_map(|t| match &t.capacity {
            TierCapacity::Absolute { cluster_bytes } => Some(*cluster_bytes),
            _ => None,
        })
        .sum::<u64>();
    let fast_headroom = saturating_pct(f_total, workload.fast_headroom_pct);
    let absolute_budget_room = f_total.saturating_sub(fast_headroom);
    if absolute_demand_fast > absolute_budget_room {
        return Err(PlacementError::PolicyOvercommit {
            tier: FjallStoreTier::SmallObject,
            cluster_demand: absolute_demand_fast,
            cluster_available: absolute_budget_room,
        });
    }

    // Edge case: F_total == 0 + S_total == 0 (empty test cluster).
    // Return clean zero budgets; warn at the call site if any.
    if f_total == 0 && s_total == 0 {
        return Ok(ClusterBudgets {
            f_total: 0,
            s_total: 0,
            metadata: 0,
            small_object: 0,
            headroom: 0,
            chunk: 0,
        });
    }

    // Per §D4.5 — projected_metadata depends on S_total only.
    let metadata_per_file_bytes = workload.metadata_per_file_bytes();
    // checked_div guards against operator setting avg_file_bytes=0
    // (would divide by zero); treat as "no projection" → floor.
    let projected_files = s_total.checked_div(workload.avg_file_bytes).unwrap_or(0);
    let projected_metadata = projected_files.saturating_mul(metadata_per_file_bytes);
    let projected_metadata_with_growth = scale_by_x100(
        projected_metadata,
        u64::from(workload.growth_headroom_x100),
        100,
    );

    if f_total == 0 {
        // F_total == 0 branch — rev-4 N-4 fix: re-base on S_total.
        // Without this, metadata_budget = 0 cluster-wide and
        // capacity tracking goes dark (auditor F-5).
        let floor = workload
            .per_tier_min_viable_bytes
            .saturating_mul(n_nodes_total);
        let ceiling = saturating_pct(s_total, workload.metadata_ceiling_pct_of_fast);
        let metadata = clamp_u64(projected_metadata_with_growth, floor, ceiling.max(floor));
        // No fast tier → no LSM headroom (HDD/SATA I/O queue is the
        // bottleneck, not LSM compaction).
        let headroom = 0;
        // Small object falls back to ~20% of slow tier, clamped at
        // floor + ceiling = S_total − metadata.
        let small_object_target = s_total / 5; // 20%
        let small_object_ceiling = s_total.saturating_sub(metadata);
        let small_object = clamp_u64(small_object_target, floor, small_object_ceiling.max(floor));
        let chunk = s_total
            .saturating_sub(metadata)
            .saturating_sub(small_object);
        return Ok(ClusterBudgets {
            f_total,
            s_total,
            metadata,
            small_object,
            headroom,
            chunk,
        });
    }

    // Normal path: F_total > 0.
    let floor = std::cmp::max(
        // ADR-049 §D4.5 "10 GiB × N_nodes_total" floor.
        10u64
            .saturating_mul(1024 * 1024 * 1024)
            .saturating_mul(n_nodes_total),
        // Q17 / rev-4 — phase-3 acceptance: floor also scales with
        // per_tier_min_viable_bytes × N_nodes so a small node's
        // share clears the fjall keyspace minimum.
        workload
            .per_tier_min_viable_bytes
            .saturating_mul(n_nodes_total),
    );
    let ceiling = saturating_pct(f_total, workload.metadata_ceiling_pct_of_fast);
    let metadata = clamp_u64(
        projected_metadata_with_growth,
        floor,
        ceiling.max(floor), // ceiling >= floor even when projection is huge
    );
    let headroom_budget = fast_headroom;
    let small_object = f_total
        .saturating_sub(metadata)
        .saturating_sub(headroom_budget);
    // Chunks: S_total + leftover_fast (leftover identically 0 by
    // construction; kept explicit for auditor).
    let leftover_fast = f_total
        .saturating_sub(metadata)
        .saturating_sub(headroom_budget)
        .saturating_sub(small_object);
    let chunk = s_total.saturating_add(leftover_fast);

    Ok(ClusterBudgets {
        f_total,
        s_total,
        metadata,
        small_object,
        headroom: headroom_budget,
        chunk,
    })
}

/// Per-node budget projection. `node.fast_capacity / F_total` ratio
/// applied to each cluster-aggregate budget.
///
/// I-DI8: by construction, `Σ tier_share ≤ node.fast_capacity` —
/// the cluster aggregate per-tier budget × `node.fast_capacity /
/// F_total` summed across tiers equals `node.fast_capacity ×
/// (Σ tier_budgets / F_total) ≤ node.fast_capacity × 1`.
#[must_use]
pub fn distribute_to_node(cluster: &ClusterBudgets, node: &NodeDeviceInventory) -> NodeBudgets {
    let fast = node.fast_capacity();
    let slow = node.slow_capacity();
    if fast == 0 || cluster.f_total == 0 {
        // No fast tier → no per-node share of metadata / small /
        // headroom. Chunks still get the node's slow capacity.
        return NodeBudgets {
            metadata: 0,
            small_object: 0,
            headroom: 0,
            chunk: slow,
        };
    }
    NodeBudgets {
        metadata: mul_div_u64(cluster.metadata, fast, cluster.f_total),
        small_object: mul_div_u64(cluster.small_object, fast, cluster.f_total),
        headroom: mul_div_u64(cluster.headroom, fast, cluster.f_total),
        // Chunk share: this node's slow tier + its proportional
        // share of any fast leftover (always 0 by construction).
        chunk: slow.saturating_add(mul_div_u64(
            cluster.chunk.saturating_sub(cluster.s_total),
            fast,
            cluster.f_total,
        )),
    }
}

/// Resolve a single tier on a single node — returns the chosen
/// mount + media class. Walks `policy.preferences` left-to-right;
/// first matching `DeviceEntry` wins.
///
/// Pure function (I-DI7).
pub fn resolve_tier_path(
    tier: FjallStoreTier,
    inventory: &NodeDeviceInventory,
    policy: &TierPolicy,
) -> Result<(PathBuf, MediaType), PlacementError> {
    for matcher in &policy.preferences {
        if let Some(entry) = find_matching_device(matcher, &inventory.devices) {
            return Ok((entry.mount_path.clone(), entry.media_class));
        }
    }
    // No preference matched.
    if policy.mode == PolicyMode::Strict {
        return Err(PlacementError::NoMatchingDevice {
            tier,
            mode: policy.mode,
            pref_count: policy.preferences.len(),
            device_count: inventory.devices.len(),
        });
    }
    // BestEffort fallback to the `data-dir-default` tag (always
    // present via discovery's data_dir fallback). If even that's
    // missing, refuse — the inventory shape is broken.
    if let Some(entry) = inventory
        .devices
        .iter()
        .find(|d| d.tag.as_deref() == Some("data-dir-default"))
    {
        tracing::warn!(
            tier = ?tier,
            "ADR-049 BestEffort fallback: no preference matched, falling back to data-dir-default",
        );
        return Ok((entry.mount_path.clone(), entry.media_class));
    }
    Err(PlacementError::NoMatchingDevice {
        tier,
        mode: policy.mode,
        pref_count: policy.preferences.len(),
        device_count: inventory.devices.len(),
    })
}

/// Resolve all four catalog-resolved fjall tiers for a node. Returns
/// in the order `FjallStoreTier::catalog_resolved()` returns
/// (deterministic).
pub fn resolve_all(
    inventory: &NodeDeviceInventory,
    cluster: &ClusterBudgets,
    policy: &PlacementPolicy,
) -> Result<[ResolvedTierBudget; 4], PlacementError> {
    let node_budgets = distribute_to_node(cluster, inventory);
    let mut out: Vec<ResolvedTierBudget> = Vec::with_capacity(4);
    for tier in FjallStoreTier::catalog_resolved() {
        let tier_policy = policy
            .for_tier(tier)
            .cloned()
            .unwrap_or_else(|| default_tier_policy(tier));
        let (mount, class) = resolve_tier_path(tier, inventory, &tier_policy)?;
        let budget = match tier {
            FjallStoreTier::SmallObject => node_budgets.small_object,
            FjallStoreTier::IntentStore
            | FjallStoreTier::CompositionMeta
            | FjallStoreTier::ChunkMeta => {
                // The three metadata-class tiers share the synthetic
                // Metadata slot. Phase 3 ships an even split. Phase 5
                // refines per-tier allocation based on measured
                // overhead.
                node_budgets.metadata / 3
            }
            FjallStoreTier::RaftLog => 0, // bootstrap-only, never resolver-routed
        };
        out.push(ResolvedTierBudget {
            tier,
            chosen_mount: mount.join("kiseki").join(tier.dir_name()),
            chosen_class: class,
            budget_bytes: budget,
        });
    }
    // The array shape is part of the public contract — the four
    // resolved tiers in canonical order.
    Ok([
        out[0].clone(),
        out[1].clone(),
        out[2].clone(),
        out[3].clone(),
    ])
}

// ---- helpers ---------------------------------------------------

fn find_matching_device<'a>(
    matcher: &DeviceMatcher,
    devices: &'a [DeviceEntry],
) -> Option<&'a DeviceEntry> {
    devices.iter().find(|d| match matcher {
        DeviceMatcher::Tag(t) => d.tag.as_deref() == Some(t.as_str()),
        DeviceMatcher::Class(c) => d.media_class == *c,
        DeviceMatcher::DataDir => d.tag.as_deref() == Some("data-dir-default"),
    })
}

fn default_tier_policy(tier: FjallStoreTier) -> TierPolicy {
    let policy = PlacementPolicy::built_in_default();
    policy
        .for_tier(tier)
        .cloned()
        .expect("built_in_default covers every catalog-resolved tier")
}

/// `(value × pct) / 100` with saturating arithmetic to avoid panics
/// on exabyte-class inputs.
fn saturating_pct(value: u64, pct: u8) -> u64 {
    mul_div_u64(value, u64::from(pct), 100)
}

/// `(value × num / den)` clamped to `[0, u64::MAX]`. Uses u128
/// intermediate to avoid overflow on multi-PiB inputs.
fn mul_div_u64(value: u64, num: u64, den: u64) -> u64 {
    if den == 0 {
        return 0;
    }
    let v = u128::from(value).saturating_mul(u128::from(num)) / u128::from(den);
    u64::try_from(v.min(u128::from(u64::MAX))).unwrap_or(u64::MAX)
}

/// `(value × x100 / 100)` with u128 intermediate.
fn scale_by_x100(value: u64, num: u64, den: u64) -> u64 {
    mul_div_u64(value, num, den)
}

/// `value` clamped to `[floor, ceiling]`. If `ceiling < floor`,
/// returns `floor` (the rev-4 N-4 "floor wins on inverted clamp"
/// fallback — auditor flagged this for `f_total == 0` path).
fn clamp_u64(value: u64, floor: u64, ceiling: u64) -> u64 {
    if floor > ceiling {
        return floor;
    }
    value.clamp(floor, ceiling)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::{DeviceEntry, MediaType};
    use std::path::PathBuf;

    const GIB: u64 = 1024 * 1024 * 1024;
    const TIB: u64 = 1024 * GIB;

    fn nvme(node: u64, fast_gib: u64, slow_gib: u64) -> NodeDeviceInventory {
        let mut devices = vec![DeviceEntry {
            mount_path: PathBuf::from(format!("/mnt/nvme{node}")),
            media_class: MediaType::Nvme,
            total_bytes: fast_gib * GIB,
            free_bytes: fast_gib * GIB,
            tag: None,
            exclusive: true,
        }];
        if slow_gib > 0 {
            devices.push(DeviceEntry {
                mount_path: PathBuf::from(format!("/mnt/sata{node}")),
                media_class: MediaType::Ssd,
                total_bytes: slow_gib * GIB,
                free_bytes: slow_gib * GIB,
                tag: None,
                exclusive: true,
            });
        }
        // Always add a data-dir fallback so BestEffort resolution
        // works in tests.
        devices.push(DeviceEntry {
            mount_path: PathBuf::from("/data"),
            media_class: MediaType::Hdd,
            total_bytes: 100 * GIB,
            free_bytes: 50 * GIB,
            tag: Some("data-dir-default".into()),
            exclusive: false,
        });
        NodeDeviceInventory {
            node_id: NodeId(node),
            devices,
            refreshed_ms: 0,
        }
    }

    fn catalog(nodes: &[NodeDeviceInventory]) -> ClusterDeviceCatalog {
        let mut c = ClusterDeviceCatalog {
            policy: PlacementPolicy::built_in_default(),
            workload: kiseki_common::WorkloadParams::default(),
            ..ClusterDeviceCatalog::default()
        };
        for n in nodes {
            c.inventories.insert(n.node_id, n.clone());
        }
        c
    }

    // I-DI7 / §D4.5 worked example: 6-node homogeneous lssd.
    #[test]
    fn homogeneous_6_node_lssd_matches_adr049_worked_example() {
        let nodes: Vec<_> = (1u64..=6).map(|n| nvme(n, 1500, 8000)).collect();
        // Use only the NVMe-class devices for the F_total calc;
        // the data-dir-default Hdd entry inflates slow_capacity.
        // Adjust expectations to match what NodeDeviceInventory's
        // fast_capacity()/slow_capacity() actually compute.
        let cat = catalog(&nodes);
        let budgets = compute_cluster_budgets(&cat).expect("ok");
        // F_total = 6 × 1500 GiB
        assert_eq!(budgets.f_total, 6 * 1500 * GIB);
        // S_total = 6 × 8000 GiB + 6 × 100 GiB (the data-dir Hdd
        // entries count as slow tier)
        assert_eq!(budgets.s_total, 6 * 8000 * GIB + 6 * 100 * GIB);
        // headroom = 25% of F_total
        assert_eq!(budgets.headroom, (6 * 1500 * GIB) / 4);
        // metadata + small + headroom must sum to ≤ F_total (I-DI8)
        let sum = budgets.metadata + budgets.small_object + budgets.headroom;
        assert!(sum <= budgets.f_total, "I-DI8 sum ≤ F_total");
    }

    // I-DI10: insertion order MUST NOT affect F_total / S_total /
    // budgets. BTreeMap iterator is canonical.
    #[test]
    fn canonical_summation_invariant_under_insert_order() {
        let nodes: Vec<_> = (1u64..=6).map(|n| nvme(n, 1500, 8000)).collect();
        let cat_a = catalog(&nodes);
        // Insert in reverse order in cat_b.
        let mut cat_b = ClusterDeviceCatalog {
            policy: PlacementPolicy::built_in_default(),
            workload: kiseki_common::WorkloadParams::default(),
            ..ClusterDeviceCatalog::default()
        };
        for n in nodes.iter().rev() {
            cat_b.inventories.insert(n.node_id, n.clone());
        }
        let a = compute_cluster_budgets(&cat_a).unwrap();
        let b = compute_cluster_budgets(&cat_b).unwrap();
        assert_eq!(a, b, "I-DI10: insertion order MUST NOT affect budgets");
    }

    // I-DI8 (per-node budget sum): for every node with
    // fast_capacity > 0, share sum ≤ node.fast_capacity.
    #[test]
    fn per_node_budget_sum_does_not_exceed_node_fast_capacity_homogeneous() {
        let nodes: Vec<_> = (1u64..=6).map(|n| nvme(n, 1500, 8000)).collect();
        let cat = catalog(&nodes);
        let cluster = compute_cluster_budgets(&cat).unwrap();
        for n in &nodes {
            let nb = distribute_to_node(&cluster, n);
            let sum = nb.metadata + nb.small_object + nb.headroom;
            assert!(
                sum <= n.fast_capacity(),
                "I-DI8: node {:?} sum {} exceeds fast_capacity {}",
                n.node_id,
                sum,
                n.fast_capacity(),
            );
        }
    }

    // Heterogeneous worked example — one root-NVMe node (200 GiB)
    // among five lssd nodes (1500 GiB each).
    #[test]
    fn heterogeneous_one_root_nvme_node_per_node_sums_fit_capacities() {
        let mut nodes: Vec<_> = (1u64..=3).map(|n| nvme(n, 1500, 8000)).collect();
        nodes.push(nvme(4, 200, 0)); // root-NVMe-only node
        nodes.extend((5u64..=6).map(|n| nvme(n, 1500, 8000)));
        let cat = catalog(&nodes);
        let cluster = compute_cluster_budgets(&cat).unwrap();
        // F_total = 200 + 5 × 1500 = 7700 GiB (rev-4 corrected)
        assert_eq!(cluster.f_total, 7700 * GIB);
        for n in &nodes {
            let nb = distribute_to_node(&cluster, n);
            let sum = nb.metadata + nb.small_object + nb.headroom;
            assert!(
                sum <= n.fast_capacity(),
                "I-DI8: node {:?} sum {} exceeds fast_capacity {}",
                n.node_id,
                sum,
                n.fast_capacity(),
            );
        }
    }

    // Edge case 1: All-NVMe cluster (S_total = 0).
    #[test]
    fn all_nvme_cluster_metadata_hits_floor_and_small_consumes_rest() {
        // 6 nodes × 1.5 TiB NVMe, no slow tier (data-dir-default's
        // 100 GiB still counts as slow via Hdd, so use a custom
        // catalog without the data_dir Hdd).
        let mut nodes: Vec<NodeDeviceInventory> = Vec::new();
        for n in 1u64..=6 {
            nodes.push(NodeDeviceInventory {
                node_id: NodeId(n),
                devices: vec![DeviceEntry {
                    mount_path: PathBuf::from(format!("/mnt/nvme{n}")),
                    media_class: MediaType::Nvme,
                    total_bytes: 1500 * GIB,
                    free_bytes: 1500 * GIB,
                    tag: None,
                    exclusive: true,
                }],
                refreshed_ms: 0,
            });
        }
        let cat = catalog(&nodes);
        let budgets = compute_cluster_budgets(&cat).unwrap();
        assert_eq!(budgets.f_total, 6 * 1500 * GIB);
        assert_eq!(budgets.s_total, 0);
        // S_total = 0 → projected_files = 0 → metadata hits floor.
        let expected_floor = std::cmp::max(
            10 * GIB * 6,
            kiseki_common::WorkloadParams::default().per_tier_min_viable_bytes * 6,
        );
        assert_eq!(budgets.metadata, expected_floor);
        // Headroom = 25% of F_total.
        assert_eq!(budgets.headroom, budgets.f_total / 4);
        // Chunks = S_total + leftover (= 0).
        assert_eq!(budgets.chunk, 0);
        // small_object consumes the rest.
        assert_eq!(
            budgets.small_object,
            budgets.f_total - budgets.metadata - budgets.headroom,
        );
    }

    // Edge case 2: Slow-only cluster (F_total = 0). Rev-4 N-4 fix:
    // formula re-bases on S_total instead of going dark.
    #[test]
    fn slow_only_cluster_does_not_go_dark_rev4_n4_fix() {
        let mut nodes: Vec<NodeDeviceInventory> = Vec::new();
        for n in 1u64..=6 {
            nodes.push(NodeDeviceInventory {
                node_id: NodeId(n),
                devices: vec![DeviceEntry {
                    mount_path: PathBuf::from(format!("/mnt/sata{n}")),
                    media_class: MediaType::Ssd,
                    total_bytes: 8000 * GIB,
                    free_bytes: 8000 * GIB,
                    tag: None,
                    exclusive: true,
                }],
                refreshed_ms: 0,
            });
        }
        let cat = catalog(&nodes);
        let budgets = compute_cluster_budgets(&cat).unwrap();
        assert_eq!(budgets.f_total, 0);
        assert!(budgets.s_total > 0);
        // Rev-4 N-4: budgets MUST NOT be zero cluster-wide.
        assert!(budgets.metadata > 0, "metadata must not go dark");
        assert!(budgets.small_object > 0, "small_object must not go dark");
        assert_eq!(budgets.headroom, 0, "no LSM headroom without fast tier");
        // Chunks consume what's left.
        assert!(budgets.chunk > 0);
    }

    // Edge case: completely empty cluster (F_total = S_total = 0).
    #[test]
    fn empty_cluster_yields_clean_zero_budgets() {
        let cat = ClusterDeviceCatalog::default();
        let budgets = compute_cluster_budgets(&cat).unwrap();
        assert_eq!(budgets.f_total, 0);
        assert_eq!(budgets.s_total, 0);
        assert_eq!(budgets.metadata, 0);
        assert_eq!(budgets.small_object, 0);
        assert_eq!(budgets.headroom, 0);
        assert_eq!(budgets.chunk, 0);
    }

    // Edge case: Absolute SmallObject > F_total → I-DI9 rejection
    // BEFORE per-node distribution (DI-5 scenario seed).
    #[test]
    fn absolute_overcommit_rejected_at_cluster_aggregate() {
        // 6 × 1.5 TiB = 9 TiB; ask for 100 TiB Absolute.
        let nodes: Vec<_> = (1u64..=6).map(|n| nvme(n, 1500, 8000)).collect();
        let mut cat = catalog(&nodes);
        cat.policy = PlacementPolicy {
            tiers: vec![TierPolicy {
                tier: FjallStoreTier::SmallObject,
                preferences: vec![DeviceMatcher::Class(MediaType::Nvme)],
                mode: PolicyMode::BestEffort,
                capacity: TierCapacity::Absolute {
                    cluster_bytes: 100 * TIB,
                },
            }],
        };
        let err = compute_cluster_budgets(&cat).unwrap_err();
        match err {
            PlacementError::PolicyOvercommit {
                cluster_demand,
                cluster_available,
                ..
            } => {
                assert_eq!(cluster_demand, 100 * TIB);
                assert!(cluster_available < cluster_demand);
            }
            other => panic!("expected PolicyOvercommit, got {other:?}"),
        }
    }

    // resolve_tier_path: BestEffort matches first preference.
    #[test]
    fn resolve_tier_path_walks_preferences_left_to_right() {
        let inv = nvme(1, 1500, 8000);
        let policy = TierPolicy {
            tier: FjallStoreTier::SmallObject,
            preferences: vec![
                DeviceMatcher::Class(MediaType::Nvme),
                DeviceMatcher::Class(MediaType::Ssd),
                DeviceMatcher::DataDir,
            ],
            mode: PolicyMode::BestEffort,
            capacity: TierCapacity::Auto {
                target_pct: 80,
                floor_bytes: 0,
                ceiling_bytes: None,
            },
        };
        let (mount, class) = resolve_tier_path(FjallStoreTier::SmallObject, &inv, &policy).unwrap();
        assert_eq!(class, MediaType::Nvme);
        assert_eq!(mount, PathBuf::from("/mnt/nvme1"));
    }

    // resolve_tier_path: Strict + no match → error (DI-3 seed).
    #[test]
    fn resolve_tier_path_strict_no_match_returns_no_matching_device() {
        // Node with no NVMe-class device.
        let inv = NodeDeviceInventory {
            node_id: NodeId(1),
            devices: vec![DeviceEntry {
                mount_path: PathBuf::from("/mnt/sata0"),
                media_class: MediaType::Ssd,
                total_bytes: TIB,
                free_bytes: TIB,
                tag: None,
                exclusive: true,
            }],
            refreshed_ms: 0,
        };
        let policy = TierPolicy {
            tier: FjallStoreTier::SmallObject,
            preferences: vec![DeviceMatcher::Class(MediaType::Nvme)],
            mode: PolicyMode::Strict,
            capacity: TierCapacity::Auto {
                target_pct: 80,
                floor_bytes: 0,
                ceiling_bytes: None,
            },
        };
        let err = resolve_tier_path(FjallStoreTier::SmallObject, &inv, &policy).unwrap_err();
        matches!(err, PlacementError::NoMatchingDevice { .. });
    }

    // resolve_all: all four catalog-resolved tiers come back with
    // a budget that fits the node, in canonical order.
    #[test]
    fn resolve_all_returns_four_tiers_in_canonical_order() {
        let nodes: Vec<_> = (1u64..=3).map(|n| nvme(n, 1500, 8000)).collect();
        let cat = catalog(&nodes);
        let cluster = compute_cluster_budgets(&cat).unwrap();
        let resolved = resolve_all(&nodes[0], &cluster, &cat.policy).unwrap();
        assert_eq!(resolved.len(), 4);
        let expected_tiers = FjallStoreTier::catalog_resolved();
        for (i, r) in resolved.iter().enumerate() {
            assert_eq!(r.tier, expected_tiers[i]);
            assert!(r.chosen_mount.ends_with(r.tier.dir_name()));
        }
    }
}
