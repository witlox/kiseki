//! `ControlCommand` — the entries that flow through the control-plane
//! Raft log (ADR-033 §4).
//!
//! Every mutation to the cluster's namespace shard map is a
//! `ControlCommand` submitted via `client_write` on the leader of the
//! control-plane Raft group, then deterministically applied on every
//! node. The same command sequence produces the same `HashMap` state
//! on every replica — that is what makes consensus safe to drive
//! per-shard Raft group creation locally on each node.

use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_common::{NodeDeviceInventory, PlacementPolicy, WorkloadParams};
use serde::{Deserialize, Serialize};

/// One mutation to the cluster's namespace shard map.
///
/// All variants are idempotent on replay so the state machine can
/// safely apply the same command twice (e.g. on snapshot install
/// followed by tail replay) without diverging from the leader.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ControlCommand {
    /// Create a brand-new namespace with the supplied initial shards.
    /// Idempotent: ignored if the namespace already exists.
    CreateNamespace {
        /// Stable namespace identifier (e.g. bucket name or
        /// `Uuid::from_u128(1)` for the bootstrap namespace).
        namespace_id: String,
        /// Owning tenant.
        tenant_id: OrgId,
        /// One per initial shard. Pre-computed by the caller (e.g.
        /// from `compute_shard_ranges`) so the state machine itself
        /// stays free of placement policy.
        shards: Vec<ShardRecord>,
        /// Full gateway-side namespace fidelity (PR #232 review
        /// blocker). Carried in the command so that EVERY path that
        /// registers the namespace with a gateway — live apply hook,
        /// restart log replay, snapshot install, and the boot drain
        /// pass — restores the exact tier policy / size-band pools /
        /// flags the creator requested, not defaults. Without this,
        /// the apply-hook registrar racing the admin handler could
        /// silently discard the request-supplied policy.
        fidelity: NamespaceFidelity,
    },
    /// Record that a shard has been split. Removes `source_shard_id`
    /// from the namespace's shards, replaces it with two halves —
    /// `[range_start, midpoint)` keeps `source_shard_id`, and
    /// `[midpoint, range_end)` becomes `new_shard_id`. The apply hook
    /// fires on every node so the new per-shard Raft group is
    /// created locally on every replica (ADR-033 §3).
    RecordSplit {
        /// Namespace whose shard map is being mutated.
        namespace_id: String,
        /// The shard being split. Must exist in the namespace map
        /// or the command is a no-op (idempotent on replay).
        source_shard_id: ShardId,
        /// The new shard's id — caller-allocated so leader and
        /// followers all converge on the same id (every replica's
        /// apply hook needs the exact same id when it locally
        /// creates the per-shard Raft group).
        new_shard_id: ShardId,
        /// 256-bit key-range midpoint where the source's range is
        /// split. The new shard owns `[midpoint, source.range_end)`;
        /// the source keeps `[source.range_start, midpoint)`.
        midpoint: [u8; 32],
        /// Best-effort leader placement for the new shard
        /// (round-robin across nodes per ADR-033 §3).
        new_leader: NodeId,
    },
    /// Record that two adjacent shards have been merged. Extends
    /// `surviving_shard_id`'s range to the union of the two and
    /// marks `retired_shard_id` for retirement (ADR-034). The
    /// retired shard stays in the map until a follow-up
    /// `RetireShard` command, so in-flight reads can drain.
    RecordMerge {
        /// Namespace whose shard map is being mutated.
        namespace_id: String,
        /// Surviving shard — keeps its id, range expands to the
        /// union of the two inputs.
        surviving_shard_id: ShardId,
        /// Shard being retired. Stays in the map (in `Retiring`
        /// state) until `RetireShard` finalizes removal.
        retired_shard_id: ShardId,
        /// New `[range_start, range_end)` for the surviving shard.
        new_range_start: [u8; 32],
        /// Exclusive upper bound for the surviving shard.
        new_range_end: [u8; 32],
    },
    /// Permanently remove a retired shard from the namespace map.
    /// Issued after the retired shard's deltas have been drained
    /// to the survivor (ADR-034). Idempotent.
    RetireShard {
        /// Namespace whose shard map is being mutated.
        namespace_id: String,
        /// The shard to remove. No-op if not present.
        shard_id: ShardId,
    },
    /// ADR-049 — upsert this node's device inventory into the
    /// cluster catalog. Submitted at boot AND every
    /// `KISEKI_INVENTORY_REFRESH_MS` (default 60 s). Apply MUST be
    /// idempotent on identical inputs (I-DI6): re-publishing an
    /// unchanged inventory MUST NOT bump `policy_revision` and
    /// MUST NOT touch `policy_change_ms`. It MAY update
    /// `inventory_change_ms` for D10 observability.
    UpsertNodeInventory {
        /// The node this inventory belongs to.
        node_id: NodeId,
        /// Discovered + tagged devices.
        inventory: NodeDeviceInventory,
    },
    /// ADR-049 — replace the cluster placement policy. Apply
    /// runs the I-DI9 gate (re-resolve against current inventories;
    /// Strict rejects if any node would violate I-DI8). On commit,
    /// bumps `policy_revision` and `policy_change_ms` (the
    /// `await_catalog_ready` quiescence clock; rev-4 N-5 fix).
    SetPlacementPolicy {
        /// The new policy to install.
        policy: PlacementPolicy,
    },
    /// ADR-049 — replace the cluster workload parameters
    /// (capacity-formula inputs). Apply runs the I-DI9 gate.
    /// On commit, bumps `policy_revision` and `policy_change_ms`
    /// (same quiescence clock as `SetPlacementPolicy`).
    SetWorkloadParams {
        /// The new workload params to install.
        params: WorkloadParams,
    },
}

// I-K8-style courtesy: Display omits payload bytes (key ranges) so
// trace logs don't dump 32-byte boundaries on every consensus event.
impl std::fmt::Display for ControlCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CreateNamespace {
                namespace_id,
                shards,
                ..
            } => write!(
                f,
                "CreateNamespace(ns={namespace_id}, shards={})",
                shards.len()
            ),
            Self::RecordSplit {
                namespace_id,
                source_shard_id,
                new_shard_id,
                ..
            } => write!(
                f,
                "RecordSplit(ns={namespace_id}, src={:?}, new={:?})",
                source_shard_id.0, new_shard_id.0
            ),
            Self::RecordMerge {
                namespace_id,
                surviving_shard_id,
                retired_shard_id,
                ..
            } => write!(
                f,
                "RecordMerge(ns={namespace_id}, surv={:?}, ret={:?})",
                surviving_shard_id.0, retired_shard_id.0
            ),
            Self::RetireShard {
                namespace_id,
                shard_id,
            } => write!(f, "RetireShard(ns={namespace_id}, shard={:?})", shard_id.0),
            Self::UpsertNodeInventory { node_id, inventory } => write!(
                f,
                "UpsertNodeInventory(node={:?}, devices={})",
                node_id.0,
                inventory.devices.len()
            ),
            Self::SetPlacementPolicy { policy } => {
                write!(f, "SetPlacementPolicy(tiers={})", policy.tiers.len())
            }
            Self::SetWorkloadParams { params } => write!(
                f,
                "SetWorkloadParams(avg_file_bytes={}, R={}, headroom_pct={})",
                params.avg_file_bytes, params.metadata_replication, params.fast_headroom_pct,
            ),
        }
    }
}

/// Full gateway-side namespace fidelity, carried in
/// `ControlCommand::CreateNamespace` (PR #232 review blocker).
///
/// These are the `kiseki_composition::namespace::Namespace` fields
/// beyond identity (`id` / `tenant_id` / `shard_id`): they drive
/// write-path behavior (`tier_policy` → device-class spill order,
/// `size_band_pools` → `select_pool_for_write`) and access semantics
/// (`read_only`, `versioning_enabled`, `compliance_tags`). Carrying
/// them through consensus means restart log replay and snapshot
/// install re-register namespaces with the exact creation-time
/// policy — the gateway's volatile namespace map is fully
/// reconstructible from the control-plane Raft alone.
///
/// `Default` = the defaults `ensure_namespace_exists` uses for
/// first-touch namespaces (no policy, writable, unversioned).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct NamespaceFidelity {
    /// Whether the namespace is read-only.
    pub read_only: bool,
    /// Whether object versioning is enabled.
    pub versioning_enabled: bool,
    /// Compliance tags applied at the namespace level.
    pub compliance_tags: Vec<kiseki_composition::namespace::ComplianceTag>,
    /// Placement-tier policy (ADR-045 §D3). Empty = default
    /// fastest-fit.
    pub tier_policy: Vec<kiseki_composition::namespace::TierQuota>,
    /// Per-size-band pool selector (ADR-024 amendment). Empty =
    /// cluster default chain.
    pub size_band_pools: kiseki_composition::namespace::NamespaceSizeBandPools,
}

impl NamespaceFidelity {
    /// Build the full composition `Namespace` this fidelity plus the
    /// given identity describes.
    #[must_use]
    pub fn to_namespace(
        &self,
        id: kiseki_common::ids::NamespaceId,
        tenant_id: OrgId,
        shard_id: ShardId,
    ) -> kiseki_composition::namespace::Namespace {
        kiseki_composition::namespace::Namespace {
            id,
            tenant_id,
            shard_id,
            read_only: self.read_only,
            versioning_enabled: self.versioning_enabled,
            compliance_tags: self.compliance_tags.clone(),
            tier_policy: self.tier_policy.clone(),
            size_band_pools: self.size_band_pools.clone(),
        }
    }
}

/// One shard's entry in the namespace map at the moment of a
/// `CreateNamespace` command. Carried in the command (rather than
/// computed in the apply hook) so the state machine stays free of
/// placement policy and apply remains deterministic.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShardRecord {
    /// Stable shard id.
    pub shard_id: ShardId,
    /// Inclusive lower bound of the 256-bit key range.
    pub range_start: [u8; 32],
    /// Exclusive upper bound of the 256-bit key range.
    pub range_end: [u8; 32],
    /// Best-effort leader placement.
    pub leader_node: NodeId,
}
