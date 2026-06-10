//! State machine for the control-plane Raft group (ADR-033 §4).
//!
//! Holds `HashMap<namespace_id, NamespaceShardMapSnapshot>` — a
//! deterministic projection of the namespace topology that every
//! replica converges on. The snapshot type intentionally lives next
//! to the state machine (not in `kiseki-control`) because the apply
//! hook serializes it onto the Raft log; reusing
//! `kiseki_control::shard_topology::NamespaceShardMap` would couple
//! the dependency-firewalled control crate to openraft (which it
//! must not depend on per the ADV-3 firewall comment in
//! `kiseki-control/Cargo.toml`).
//!
//! `NamespaceShardMapSnapshot` is a one-to-one reflection of the
//! production shape — gateway routing code can convert a snapshot
//! back into `NamespaceShardMap` via `to_topology()` without
//! semantic loss.

use std::collections::HashMap;
use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures::TryStreamExt;
use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_common::ClusterDeviceCatalog;
use kiseki_control::shard_topology::{
    NamespaceCreationState, NamespaceShardMap, NamespaceShardMapStore, ShardRange,
};
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder};
use serde::{Deserialize, Serialize};

use super::commands::{ControlCommand, ShardRecord};
use super::store::ApplyHook;
use super::types::{ControlResponse, ControlTypeConfig};

type C = ControlTypeConfig;

/// One namespace's shard map as carried in the control-plane Raft
/// state machine. Mirrors `kiseki_control::NamespaceShardMap` but
/// owns its own serde derives so it can ride the Raft log + snapshot
/// stream without imposing those on the dependency-firewalled
/// `kiseki-control` crate.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NamespaceShardMapSnapshot {
    /// Stable namespace identifier.
    pub namespace_id: String,
    /// Owning tenant.
    pub tenant_id: OrgId,
    /// Monotonically increasing version on every successful apply.
    /// Useful for cache-invalidation: a routing-cache entry whose
    /// observed version is less than the state machine's is stale.
    pub version: u64,
    /// One per shard. Sorted by `range_start`. May contain a
    /// retired-but-not-yet-removed shard (post-merge) until the
    /// follow-up `RetireShard` command lands.
    pub shards: Vec<ShardSnapshot>,
}

/// One shard's record. `is_retiring` marks the post-merge state
/// where the shard's range has already been absorbed into the
/// surviving shard but the shard hasn't been finalized for removal.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ShardSnapshot {
    /// Stable shard id.
    pub shard_id: ShardId,
    /// Inclusive lower bound of the 256-bit key range.
    pub range_start: [u8; 32],
    /// Exclusive upper bound of the 256-bit key range.
    pub range_end: [u8; 32],
    /// Best-effort leader placement.
    pub leader_node: NodeId,
    /// Set on `RecordMerge` for the retired side. Cleared by
    /// `RetireShard` (which removes the shard entirely).
    pub is_retiring: bool,
}

impl NamespaceShardMapSnapshot {
    /// Convert this snapshot to the production
    /// `kiseki_control::NamespaceShardMap`. Caller-side use only —
    /// the conversion is lossy on `is_retiring` (the production type
    /// has no equivalent today; gateways treat retiring shards as
    /// regular shards for routing during the drain window).
    #[must_use]
    #[allow(dead_code)] // wired when admin RPC reads land
    pub fn to_topology(&self) -> NamespaceShardMap {
        NamespaceShardMap {
            namespace_id: self.namespace_id.clone(),
            tenant_id: self.tenant_id,
            version: self.version,
            shards: self
                .shards
                .iter()
                .map(|s| ShardRange {
                    shard_id: s.shard_id,
                    range_start: s.range_start,
                    range_end: s.range_end,
                    leader_node: s.leader_node,
                })
                .collect(),
            // Once the namespace exists in the control-plane Raft
            // state machine it is by definition Active — partial
            // creates are rolled back by the leader before the
            // CreateNamespace command commits (ADV-033-1).
            state: NamespaceCreationState::Active,
        }
    }
}

/// One snapshot of the entire control-plane state — used both for
/// openraft's snapshot install/build path and for unit tests that
/// want to deterministically inspect the post-apply state.
///
/// ADR-049 phase 1 added `catalog` with `#[serde(default)]` so a
/// pre-upgrade snapshot (no `catalog` field) decodes cleanly into
/// `ClusterDeviceCatalog::default()`. Subsequent
/// `UpsertNodeInventory` / `SetPlacementPolicy` /
/// `SetWorkloadParams` applies then populate it. Q36 acceptance.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ControlSnapshot {
    /// All namespaces known to the control plane.
    pub namespaces: HashMap<String, NamespaceShardMapSnapshot>,
    /// ADR-049 cluster device catalog. `#[serde(default)]` keeps
    /// pre-upgrade snapshots decodable.
    #[serde(default)]
    pub catalog: ClusterDeviceCatalog,
}

/// Inner state shared between the `RaftStateMachine` impl and
/// outside readers (e.g. routing). Wrapped in
/// `futures::lock::Mutex` for compatibility with the openraft
/// `apply` future.
pub(crate) struct StateMachineInner {
    pub(crate) namespaces: HashMap<String, NamespaceShardMapSnapshot>,
    /// ADR-049 cluster device catalog. Mutated by
    /// `UpsertNodeInventory` / `SetPlacementPolicy` /
    /// `SetWorkloadParams` applies. Read by every node's
    /// `await_catalog_ready` + resolver at boot.
    pub(crate) catalog: ClusterDeviceCatalog,
    pub(crate) last_applied_log: Option<LogIdOf<C>>,
    pub(crate) last_membership: StoredMembershipOf<C>,
}

impl StateMachineInner {
    pub(crate) fn new() -> Self {
        Self {
            namespaces: HashMap::new(),
            catalog: ClusterDeviceCatalog::default(),
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
        }
    }

    #[allow(clippy::too_many_lines)] // 4 idempotent variants — splitting hides the parallel structure
    fn apply_command(&mut self, cmd: &ControlCommand) -> ControlResponse {
        match cmd {
            ControlCommand::CreateNamespace {
                namespace_id,
                tenant_id,
                shards,
            } => {
                if self.namespaces.contains_key(namespace_id) {
                    // Idempotent on replay: ignore re-creation. The
                    // count returned matches what's already in the
                    // map so admin RPCs see a stable response.
                    let existing_count = u32::try_from(self.namespaces[namespace_id].shards.len())
                        .unwrap_or(u32::MAX);
                    return ControlResponse::NamespaceCreated {
                        shard_count: existing_count,
                    };
                }
                let mut shards: Vec<ShardSnapshot> =
                    shards.iter().map(shard_record_to_snapshot).collect();
                shards.sort_by_key(|s| s.range_start);
                let count = u32::try_from(shards.len()).unwrap_or(u32::MAX);
                self.namespaces.insert(
                    namespace_id.clone(),
                    NamespaceShardMapSnapshot {
                        namespace_id: namespace_id.clone(),
                        tenant_id: *tenant_id,
                        version: 1,
                        shards,
                    },
                );
                ControlResponse::NamespaceCreated { shard_count: count }
            }
            ControlCommand::RecordSplit {
                namespace_id,
                source_shard_id,
                new_shard_id,
                midpoint,
                new_leader,
            } => {
                let Some(map) = self.namespaces.get_mut(namespace_id) else {
                    // Idempotent: unknown namespace is a no-op (it
                    // may have been retired by a later command).
                    return ControlResponse::SplitRecorded {
                        new_shard_id: *new_shard_id,
                    };
                };
                // Idempotent on replay: if the new shard already
                // exists, skip the mutation.
                if map.shards.iter().any(|s| s.shard_id == *new_shard_id) {
                    return ControlResponse::SplitRecorded {
                        new_shard_id: *new_shard_id,
                    };
                }
                let Some(idx) = map
                    .shards
                    .iter()
                    .position(|s| s.shard_id == *source_shard_id)
                else {
                    // Source not found — possibly already split
                    // (idempotent) or never existed; accept either
                    // way to keep apply deterministic on replay.
                    return ControlResponse::SplitRecorded {
                        new_shard_id: *new_shard_id,
                    };
                };
                let source = &mut map.shards[idx];
                let upper_end = source.range_end;
                source.range_end = *midpoint;
                let new_shard = ShardSnapshot {
                    shard_id: *new_shard_id,
                    range_start: *midpoint,
                    range_end: upper_end,
                    leader_node: *new_leader,
                    is_retiring: false,
                };
                map.shards.push(new_shard);
                map.shards.sort_by_key(|s| s.range_start);
                map.version += 1;
                ControlResponse::SplitRecorded {
                    new_shard_id: *new_shard_id,
                }
            }
            ControlCommand::RecordMerge {
                namespace_id,
                surviving_shard_id,
                retired_shard_id,
                new_range_start,
                new_range_end,
            } => {
                let Some(map) = self.namespaces.get_mut(namespace_id) else {
                    return ControlResponse::Applied;
                };
                if let Some(s) = map
                    .shards
                    .iter_mut()
                    .find(|s| s.shard_id == *surviving_shard_id)
                {
                    s.range_start = *new_range_start;
                    s.range_end = *new_range_end;
                }
                if let Some(s) = map
                    .shards
                    .iter_mut()
                    .find(|s| s.shard_id == *retired_shard_id)
                {
                    s.is_retiring = true;
                }
                map.shards.sort_by_key(|s| s.range_start);
                map.version += 1;
                ControlResponse::Applied
            }
            ControlCommand::RetireShard {
                namespace_id,
                shard_id,
            } => {
                if let Some(map) = self.namespaces.get_mut(namespace_id) {
                    let before = map.shards.len();
                    map.shards.retain(|s| s.shard_id != *shard_id);
                    if map.shards.len() != before {
                        map.version += 1;
                    }
                }
                ControlResponse::Applied
            }
            ControlCommand::UpsertNodeInventory { node_id, inventory } => {
                // ADR-049 I-DI6: idempotent on identical inputs. Compare
                // against current state; skip the mutation if unchanged
                // so a re-publish during refresh churn does NOT bump
                // `inventory_change_ms`.
                let changed = self
                    .catalog
                    .inventories
                    .get(node_id)
                    .is_none_or(|cur| cur != inventory);
                if changed {
                    self.catalog.inventories.insert(*node_id, inventory.clone());
                    // `inventory_change_ms` is for D10 observability
                    // only and explicitly NOT the quiescence clock
                    // (rev-4 N-5 fix). `policy_change_ms` stays put.
                    self.catalog.inventory_change_ms = now_ms();
                }
                ControlResponse::Applied
            }
            ControlCommand::SetPlacementPolicy { policy } => {
                // ADR-049 I-DI9 (apply-time gate): re-resolve budgets
                // against current inventories under the new policy;
                // reject the LogCommand at apply if the cluster-
                // aggregate Absolute pre-check trips. Per-node I-DI8
                // checks land alongside the BestEffort `policy_apply_
                // rebudget` event stream in phase 4 admin RPC.
                //
                // Deterministic: every replica's `catalog.inventories`
                // converges via prior `UpsertNodeInventory` applies;
                // every replica's I-DI9 evaluation against that state
                // produces the same decision. Implementer property
                // test: two distinct replicas applying the same
                // ControlCommand from the same prior state MUST
                // agree on accept-or-reject.
                if self.catalog.policy == *policy {
                    ControlResponse::Applied
                } else {
                    let mut probe = self.catalog.clone();
                    probe.policy = policy.clone();
                    match crate::cluster_control::resolver::compute_cluster_budgets(&probe) {
                        Ok(_) => {
                            self.catalog.policy = policy.clone();
                            self.catalog.policy_revision =
                                self.catalog.policy_revision.saturating_add(1);
                            self.catalog.policy_change_ms = now_ms();
                            ControlResponse::Applied
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "ADR-049 I-DI9: SetPlacementPolicy rejected at apply time"
                            );
                            ControlResponse::PolicyRejected {
                                reason: format!("{err}"),
                            }
                        }
                    }
                }
            }
            ControlCommand::SetWorkloadParams { params } => {
                // I-DI9 also fires for workload-param changes:
                // tweaking `avg_file_bytes` or `growth_headroom` can
                // shift `metadata_budget` past the ceiling and
                // overcommit Absolute SmallObject if any tier is
                // Absolute. Same evaluate-then-apply shape.
                if self.catalog.workload == *params {
                    ControlResponse::Applied
                } else {
                    let mut probe = self.catalog.clone();
                    probe.workload = *params;
                    match crate::cluster_control::resolver::compute_cluster_budgets(&probe) {
                        Ok(_) => {
                            self.catalog.workload = *params;
                            self.catalog.policy_revision =
                                self.catalog.policy_revision.saturating_add(1);
                            self.catalog.policy_change_ms = now_ms();
                            ControlResponse::Applied
                        }
                        Err(err) => {
                            tracing::warn!(
                                error = %err,
                                "ADR-049 I-DI9: SetWorkloadParams rejected at apply time"
                            );
                            ControlResponse::PolicyRejected {
                                reason: format!("{err}"),
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Wall-clock helper used by ADR-049 apply paths.
/// Returns 0 if `SystemTime::now()` is before the epoch (impossible
/// on a sane host, but the apply path must not panic).
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn shard_record_to_snapshot(r: &ShardRecord) -> ShardSnapshot {
    ShardSnapshot {
        shard_id: r.shard_id,
        range_start: r.range_start,
        range_end: r.range_end,
        leader_node: r.leader_node,
        is_retiring: false,
    }
}

/// openraft state machine implementation for the control-plane group.
///
/// Holds the apply hook so it fires on every node from `apply()` —
/// not only on the leader. That is what makes ADR-033 §4 cluster-
/// wide: a single consensus event drives per-shard Raft group
/// creation on every replica, deterministically and in lockstep
/// with the namespace shard map mutation.
#[derive(Clone)]
pub struct ControlStateMachine {
    pub(crate) inner: Arc<futures::lock::Mutex<StateMachineInner>>,
    /// Apply hook fired AFTER each command has been applied to
    /// `inner`. `None` for unit tests that don't wire side effects.
    apply_hook: Option<Arc<dyn ApplyHook>>,
    /// Optional metrics: per-replica apply counts, namespace
    /// gauge, apply-hook duration. `None` for unit tests.
    metrics: Option<Arc<super::ClusterControlMetrics>>,
    /// Gateway-readable `NamespaceShardMapStore` kept in lockstep with
    /// the state machine's `namespaces` map. Hydrated on every node's
    /// apply path so the gateway's `shard_map` lookup
    /// (`mem_gateway::route_to_shard`) routes by `hashed_key` range
    /// rather than the namespace's primary `comp.shard_id`.
    ///
    /// Without this wiring the gateway's `shard_map` is `None` in
    /// production and ADR-033 §5 routing is dead code — every write
    /// goes to the single shard `comps.add_namespace` registered.
    /// `None` for unit tests and single-node setups that don't
    /// engage the control plane.
    shard_map: Option<Arc<NamespaceShardMapStore>>,
}

impl ControlStateMachine {
    /// Build a fresh state machine with no apply hook (tests).
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(futures::lock::Mutex::new(StateMachineInner::new())),
            apply_hook: None,
            metrics: None,
            shard_map: None,
        }
    }

    /// Build with an apply hook bound. The hook fires on every node
    /// after each command's state mutation completes — see
    /// [`ApplyHook`] for semantics.
    #[must_use]
    pub fn with_apply_hook(hook: Arc<dyn ApplyHook>) -> Self {
        Self {
            inner: Arc::new(futures::lock::Mutex::new(StateMachineInner::new())),
            apply_hook: Some(hook),
            metrics: None,
            shard_map: None,
        }
    }

    /// Builder: attach the prometheus metrics struct so per-replica
    /// `apply` and `apply_hook_duration` tick.
    #[must_use]
    pub fn with_apply_metrics(mut self, metrics: Arc<super::ClusterControlMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Builder: attach the gateway-readable `NamespaceShardMapStore`
    /// so every `CreateNamespace` apply populates it on every node.
    /// This is what makes ADR-033 §5 routing (`route_to_shard` over
    /// `hashed_key` ranges) actually engage in production — without
    /// this wiring the gateway's `shard_map` is `None` and writes
    /// route to the namespace's single primary `comp.shard_id`.
    #[must_use]
    pub fn with_shard_map(mut self, shard_map: Arc<NamespaceShardMapStore>) -> Self {
        self.shard_map = Some(shard_map);
        self
    }

    /// Mirror one `ControlCommand` into the gateway-readable
    /// `NamespaceShardMapStore`. Called from `apply` on every node
    /// after the inner state mutation commits, and from
    /// `install_snapshot` on snapshot-driven catch-up.
    ///
    /// Errors are downgraded to `tracing::debug` — by the time we
    /// reach here the consensus log already records the canonical
    /// state, and the local store is a derived projection. Re-apply
    /// on snapshot install repopulates it.
    pub(crate) fn hydrate_shard_map(shard_map: &NamespaceShardMapStore, cmd: &ControlCommand) {
        match cmd {
            ControlCommand::CreateNamespace {
                namespace_id,
                tenant_id,
                shards,
            } => {
                // Use the caller-supplied shards verbatim. The earlier
                // implementation called `create_namespace(...,
                // Some(N))` which invented FRESH shard UUIDs via
                // `compute_shard_ranges` — the shard_map then routed
                // writes to shards that didn't exist locally because
                // the apply hook had registered the per-shard Raft
                // groups under the COMMAND's shard IDs, not the
                // store's freshly-generated ones. Observed on the
                // 2026-05-17 dev compose: every PUT against the new
                // multi-shard "default" namespace returned `shard
                // not found`.
                let ranges: Vec<kiseki_control::shard_topology::ShardRange> = shards
                    .iter()
                    .map(|s| kiseki_control::shard_topology::ShardRange {
                        shard_id: s.shard_id,
                        range_start: s.range_start,
                        range_end: s.range_end,
                        leader_node: s.leader_node,
                    })
                    .collect();
                match shard_map.register_namespace_with_shards(namespace_id, *tenant_id, ranges) {
                    Ok(_) => {
                        tracing::debug!(
                            namespace_id = %namespace_id,
                            shards = shards.len(),
                            "control-plane apply: shard_map hydrated",
                        );
                    }
                    Err(e) => {
                        // `AlreadyExists` is the expected idempotent
                        // path on replay; anything else means the
                        // local store and state machine have drifted
                        // (impossible without a bug).
                        tracing::debug!(
                            namespace_id = %namespace_id,
                            error = %e,
                            "control-plane apply: shard_map create skipped (already present?)",
                        );
                    }
                }
            }
            // Split / Merge / Retire mutate existing shards. The
            // current `NamespaceShardMapStore` API doesn't expose
            // per-shard mutation helpers — the production read path
            // tolerates a slightly-stale local store via the in-memory
            // state-machine snapshot. Follow-up: thread split/merge
            // updates through too (tracked alongside the split/merge
            // BDD scenarios).
            //
            // ADR-049 catalog mutations (`UpsertNodeInventory` /
            // `SetPlacementPolicy` / `SetWorkloadParams`) also don't
            // touch the namespace shard map; they live in
            // `ClusterDeviceCatalog` and are read directly by the
            // resolver + admin RPC.
            ControlCommand::RecordSplit { .. }
            | ControlCommand::RecordMerge { .. }
            | ControlCommand::RetireShard { .. }
            | ControlCommand::UpsertNodeInventory { .. }
            | ControlCommand::SetPlacementPolicy { .. }
            | ControlCommand::SetWorkloadParams { .. } => {}
        }
    }

    /// Snapshot of the full state — for tests and read-side admin RPCs.
    #[allow(dead_code)] // exposed for future read-side admin RPCs
    pub async fn snapshot(&self) -> ControlSnapshot {
        let inner = self.inner.lock().await;
        ControlSnapshot {
            namespaces: inner.namespaces.clone(),
            catalog: inner.catalog.clone(),
        }
    }

    /// Read-only catalog snapshot — used by the ADR-049 resolver,
    /// admin RPCs, and observability gauges; avoids cloning the
    /// whole `ControlSnapshot` when only the catalog is needed.
    #[allow(dead_code)]
    pub async fn catalog(&self) -> ClusterDeviceCatalog {
        let inner = self.inner.lock().await;
        inner.catalog.clone()
    }

    /// Get a single namespace's shard map. `None` if the namespace
    /// has not been created via `CreateNamespace` yet.
    #[allow(dead_code)] // exposed for future read-side admin RPCs
    pub async fn namespace(&self, namespace_id: &str) -> Option<NamespaceShardMapSnapshot> {
        let inner = self.inner.lock().await;
        inner.namespaces.get(namespace_id).cloned()
    }
}

impl Default for ControlStateMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl RaftSnapshotBuilder<C> for ControlStateMachine {
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C>, io::Error> {
        let inner = self.inner.lock().await;
        let snap = ControlSnapshot {
            namespaces: inner.namespaces.clone(),
            catalog: inner.catalog.clone(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let snapshot_id = format!(
            "snap-{}",
            inner
                .last_applied_log
                .as_ref()
                .map_or(0, openraft::LogId::index)
        );
        let meta = SnapshotMetaOf::<C> {
            last_log_id: inner.last_applied_log,
            last_membership: inner.last_membership.clone(),
            snapshot_id,
        };
        Ok(openraft::storage::Snapshot {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl RaftStateMachine<C> for ControlStateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf<C>>, StoredMembershipOf<C>), io::Error> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied_log, inner.last_membership.clone()))
    }

    #[tracing::instrument(skip(self, entries), level = "debug")]
    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: futures::Stream<Item = Result<EntryResponder<C>, io::Error>> + Unpin + OptionalSend,
    {
        // Collect (cmd, responder) pairs so the inner-state mutex
        // is dropped before firing the apply hook. Some hook impls
        // (e.g. `ShardStoreApplyHook`) call into `RaftShardStore`
        // which spawns its own runtime threads — keeping the inner
        // lock held across them would serialize all subsequent
        // applies behind the heaviest one. Empirically that's
        // ~50-200 ms per `create_shard`, enough to stall heartbeat
        // replication.
        let mut hook_dispatch: Vec<ControlCommand> = Vec::new();
        let namespaces_after: usize;
        {
            let mut inner = self.inner.lock().await;
            while let Some((entry, responder)) = entries.try_next().await? {
                inner.last_applied_log = Some(entry.log_id);
                let response = match &entry.payload {
                    EntryPayload::Blank => ControlResponse::Applied,
                    EntryPayload::Normal(cmd) => {
                        let r = inner.apply_command(cmd);
                        if let Some(m) = self.metrics.as_ref() {
                            m.record_apply(super::ClusterControlMetrics::op_label(cmd));
                        }
                        hook_dispatch.push(cmd.clone());
                        r
                    }
                    EntryPayload::Membership(mem) => {
                        inner.last_membership =
                            openraft::StoredMembership::new(Some(entry.log_id), mem.clone());
                        ControlResponse::Applied
                    }
                };
                if let Some(responder) = responder {
                    responder.send(response);
                }
            }
            namespaces_after = inner.namespaces.len();
        }
        // Update the namespace gauge once per apply batch; the
        // value is replicated to every node so the gauge is
        // identical across the cluster once consensus converges.
        if let Some(m) = self.metrics.as_ref() {
            m.namespaces
                .set(i64::try_from(namespaces_after).unwrap_or(i64::MAX));
        }
        // Apply hook fires AFTER state mutation. Each method on
        // `ApplyHook` is contracted to be safe against replay
        // (idempotent) and to do its own background spawning when
        // the work is slow — that contract is what keeps openraft's
        // apply task moving through subsequent entries promptly.
        if let Some(hook) = self.apply_hook.as_ref() {
            for cmd in &hook_dispatch {
                let op = super::ClusterControlMetrics::op_label(cmd);
                let started = std::time::Instant::now();
                let _span = tracing::debug_span!(
                    "cluster_control.apply_hook",
                    op,
                    cmd = %cmd,
                )
                .entered();
                super::store::OpenRaftControlStore::dispatch_hook(hook.as_ref(), cmd);
                if let Some(m) = self.metrics.as_ref() {
                    m.record_hook_duration(op, started.elapsed());
                }
            }
        }
        // ADR-033 §5: keep the gateway-readable shard map in lockstep
        // with the state machine. Runs on every node so the gateway's
        // `route_to_shard` lookup hits a populated map regardless of
        // which node fields the request. Idempotent against replay
        // (already-existing → `AlreadyExists` is swallowed below).
        if let Some(shard_map) = self.shard_map.as_ref() {
            for cmd in &hook_dispatch {
                Self::hydrate_shard_map(shard_map.as_ref(), cmd);
            }
        }
        Ok(())
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<<C as openraft::RaftTypeConfig>::SnapshotData, io::Error> {
        Ok(Cursor::new(Vec::new()))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<C>,
        snapshot: <C as openraft::RaftTypeConfig>::SnapshotData,
    ) -> Result<(), io::Error> {
        let data = snapshot.into_inner();
        let snap: ControlSnapshot = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // Replay snapshot namespaces into the gateway-readable shard
        // map. A restarting node receives the snapshot from the leader
        // and would otherwise have an empty shard_map until the next
        // CreateNamespace lands — wrong: existing namespaces should
        // be routable immediately.
        if let Some(shard_map) = self.shard_map.as_ref() {
            for (ns_id, ns_snap) in &snap.namespaces {
                let shards: Vec<ShardRecord> = ns_snap
                    .shards
                    .iter()
                    .map(|s| ShardRecord {
                        shard_id: s.shard_id,
                        range_start: s.range_start,
                        range_end: s.range_end,
                        leader_node: s.leader_node,
                    })
                    .collect();
                Self::hydrate_shard_map(
                    shard_map.as_ref(),
                    &ControlCommand::CreateNamespace {
                        namespace_id: ns_id.clone(),
                        tenant_id: ns_snap.tenant_id,
                        shards,
                    },
                );
            }
        }
        // GH #192: snapshot-driven catch-up bypasses the per-command
        // apply path, so the gateway-side namespace registration
        // (`ApplyHook::on_namespace_applied`) must fire here too —
        // otherwise a node that catches up via snapshot install has a
        // populated shard map but an empty gateway namespace registry
        // and every write 404s with `NamespaceNotFound`. Idempotent
        // (the registrar no-ops on already-registered namespaces).
        // Note: per-shard Raft group creation (`on_create_namespace`)
        // is deliberately NOT dispatched here — snapshot install for
        // shard-group state is a separate pre-existing gap tracked
        // with the split/merge BDD scenarios.
        if let Some(hook) = self.apply_hook.as_ref() {
            for (ns_id, ns_snap) in &snap.namespaces {
                hook.on_namespace_applied(ns_id, ns_snap.tenant_id);
            }
        }
        let mut inner = self.inner.lock().await;
        inner.namespaces = snap.namespaces;
        inner.catalog = snap.catalog;
        inner.last_applied_log = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        let Some(ref last) = inner.last_applied_log else {
            return Ok(None);
        };
        let snap = ControlSnapshot {
            namespaces: inner.namespaces.clone(),
            catalog: inner.catalog.clone(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let meta = SnapshotMetaOf::<C> {
            last_log_id: Some(*last),
            last_membership: inner.last_membership.clone(),
            snapshot_id: format!("snap-{}", last.index()),
        };
        Ok(Some(openraft::storage::Snapshot {
            meta,
            snapshot: Cursor::new(data),
        }))
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::{NodeId, OrgId, ShardId};
    use uuid::Uuid;

    fn ns() -> String {
        "ns-test".to_owned()
    }

    fn org() -> OrgId {
        OrgId(Uuid::from_u128(0x4242))
    }

    fn shard(byte: u8) -> ShardId {
        ShardId(Uuid::from_u128(u128::from(byte) << 120))
    }

    fn full_range(s: ShardId) -> ShardRecord {
        ShardRecord {
            shard_id: s,
            range_start: [0u8; 32],
            range_end: [0xFFu8; 32],
            leader_node: NodeId(1),
        }
    }

    /// GH #192: snapshot-driven catch-up bypasses the per-command
    /// apply path, so `install_snapshot` must fire
    /// `ApplyHook::on_namespace_applied` for every namespace in the
    /// installed snapshot — otherwise a node that catches up via
    /// snapshot has a populated shard map but an empty gateway
    /// namespace registry.
    #[tokio::test]
    async fn install_snapshot_fires_namespace_applied_for_each_namespace() {
        #[derive(Default)]
        struct RecordingHook {
            namespaces: std::sync::Mutex<Vec<(String, OrgId)>>,
        }
        impl ApplyHook for RecordingHook {
            fn on_create_namespace(&self, _: &str, _: OrgId, _: ShardId, _: NodeId) {}
            fn on_namespace_applied(&self, ns: &str, tenant_id: OrgId) {
                self.namespaces
                    .lock()
                    .unwrap()
                    .push((ns.to_owned(), tenant_id));
            }
            fn on_split(&self, _: &str, _: ShardId, _: ShardId, _: NodeId) {}
            fn on_merge(&self, _: &str, _: ShardId, _: ShardId) {}
            fn on_retire(&self, _: &str, _: ShardId) {}
        }

        let hook = std::sync::Arc::new(RecordingHook::default());
        let mut sm = ControlStateMachine::with_apply_hook(
            std::sync::Arc::clone(&hook) as Arc<dyn ApplyHook>
        );

        let mut namespaces = HashMap::new();
        namespaces.insert(
            ns(),
            NamespaceShardMapSnapshot {
                namespace_id: ns(),
                tenant_id: org(),
                version: 1,
                shards: vec![ShardSnapshot {
                    shard_id: shard(1),
                    range_start: [0u8; 32],
                    range_end: [0xFFu8; 32],
                    leader_node: NodeId(1),
                    is_retiring: false,
                }],
            },
        );
        let snap = ControlSnapshot {
            namespaces,
            catalog: ClusterDeviceCatalog::default(),
        };
        let data = serde_json::to_vec(&snap).unwrap();
        let meta = SnapshotMetaOf::<C> {
            last_log_id: None,
            last_membership: StoredMembershipOf::<C>::default(),
            snapshot_id: "snap-test".to_owned(),
        };
        sm.install_snapshot(&meta, Cursor::new(data))
            .await
            .expect("install ok");

        assert_eq!(
            hook.namespaces.lock().unwrap().as_slice(),
            &[(ns(), org())],
            "on_namespace_applied fires once per namespace on snapshot install",
        );
        // The state itself landed too.
        let post = sm.snapshot().await;
        assert!(post.namespaces.contains_key(&ns()));
    }

    #[test]
    fn create_namespace_inserts_initial_shards_and_bumps_version_to_1() {
        let mut sm = StateMachineInner::new();
        let resp = sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        assert_eq!(resp, ControlResponse::NamespaceCreated { shard_count: 1 });
        let snap = sm.namespaces.get(&ns()).expect("ns inserted");
        assert_eq!(snap.version, 1);
        assert_eq!(snap.shards.len(), 1);
        assert_eq!(snap.shards[0].shard_id, shard(1));
    }

    #[test]
    fn create_namespace_is_idempotent_on_replay() {
        let mut sm = StateMachineInner::new();
        let cmd = ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        };
        let _ = sm.apply_command(&cmd);
        let resp2 = sm.apply_command(&cmd);
        // Second apply must NOT bump version or duplicate shards —
        // openraft replays committed entries on snapshot install
        // followed by tail catch-up, and any non-idempotent apply
        // would diverge replicas.
        assert_eq!(resp2, ControlResponse::NamespaceCreated { shard_count: 1 });
        assert_eq!(sm.namespaces[&ns()].version, 1);
        assert_eq!(sm.namespaces[&ns()].shards.len(), 1);
    }

    #[test]
    fn record_split_replaces_source_with_two_halves() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        let mut midpoint = [0u8; 32];
        midpoint[0] = 0x80;
        let resp = sm.apply_command(&ControlCommand::RecordSplit {
            namespace_id: ns(),
            source_shard_id: shard(1),
            new_shard_id: shard(2),
            midpoint,
            new_leader: NodeId(2),
        });
        assert_eq!(
            resp,
            ControlResponse::SplitRecorded {
                new_shard_id: shard(2)
            }
        );
        let snap = &sm.namespaces[&ns()];
        assert_eq!(snap.shards.len(), 2);
        assert_eq!(snap.version, 2);
        assert_eq!(snap.shards[0].shard_id, shard(1));
        assert_eq!(snap.shards[0].range_end, midpoint);
        assert_eq!(snap.shards[1].shard_id, shard(2));
        assert_eq!(snap.shards[1].range_start, midpoint);
        assert_eq!(snap.shards[1].range_end, [0xFFu8; 32]);
    }

    #[test]
    fn record_split_is_idempotent_when_new_shard_already_present() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        let mut midpoint = [0u8; 32];
        midpoint[0] = 0x80;
        let cmd = ControlCommand::RecordSplit {
            namespace_id: ns(),
            source_shard_id: shard(1),
            new_shard_id: shard(2),
            midpoint,
            new_leader: NodeId(2),
        };
        sm.apply_command(&cmd);
        let v_after_first = sm.namespaces[&ns()].version;
        sm.apply_command(&cmd);
        assert_eq!(
            sm.namespaces[&ns()].version,
            v_after_first,
            "replay must not bump version a second time",
        );
        assert_eq!(sm.namespaces[&ns()].shards.len(), 2);
    }

    #[test]
    fn record_merge_marks_retired_and_extends_surviving_range() {
        let mut sm = StateMachineInner::new();
        let mut mid = [0u8; 32];
        mid[0] = 0x80;
        // Pre-split state: two shards covering [0x00…, 0xFF…)
        let s1 = ShardRecord {
            shard_id: shard(1),
            range_start: [0u8; 32],
            range_end: mid,
            leader_node: NodeId(1),
        };
        let s2 = ShardRecord {
            shard_id: shard(2),
            range_start: mid,
            range_end: [0xFFu8; 32],
            leader_node: NodeId(2),
        };
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![s1, s2],
        });
        let resp = sm.apply_command(&ControlCommand::RecordMerge {
            namespace_id: ns(),
            surviving_shard_id: shard(1),
            retired_shard_id: shard(2),
            new_range_start: [0u8; 32],
            new_range_end: [0xFFu8; 32],
        });
        assert_eq!(resp, ControlResponse::Applied);
        let snap = &sm.namespaces[&ns()];
        let surv = snap
            .shards
            .iter()
            .find(|s| s.shard_id == shard(1))
            .expect("surviving in map");
        assert_eq!(surv.range_end, [0xFFu8; 32], "surviving range expanded");
        let ret = snap
            .shards
            .iter()
            .find(|s| s.shard_id == shard(2))
            .expect("retired still in map until RetireShard");
        assert!(ret.is_retiring, "retired shard flagged");
    }

    #[test]
    fn retire_shard_finalizes_removal_after_merge() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1)), full_range(shard(2))],
        });
        let v_before = sm.namespaces[&ns()].version;
        sm.apply_command(&ControlCommand::RetireShard {
            namespace_id: ns(),
            shard_id: shard(2),
        });
        assert_eq!(sm.namespaces[&ns()].shards.len(), 1);
        assert!(sm.namespaces[&ns()].version > v_before);
        // Replay must not bump version again.
        let v_after_first = sm.namespaces[&ns()].version;
        sm.apply_command(&ControlCommand::RetireShard {
            namespace_id: ns(),
            shard_id: shard(2),
        });
        assert_eq!(sm.namespaces[&ns()].version, v_after_first);
    }

    #[test]
    fn snapshot_round_trips_through_install_path() {
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        let snap = ControlSnapshot {
            namespaces: sm.namespaces.clone(),
            catalog: sm.catalog.clone(),
        };
        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let parsed: ControlSnapshot = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(parsed.namespaces.len(), 1);
        assert_eq!(parsed.namespaces[&ns()].shards.len(), 1);
        assert_eq!(parsed.namespaces[&ns()].shards[0].shard_id, shard(1));
    }

    // ---- ADR-049 phase 1 catalog apply tests ----

    fn inventory(node: u64, fast_gib: u64, slow_gib: u64) -> kiseki_common::NodeDeviceInventory {
        use kiseki_common::{DeviceEntry, MediaType, NodeDeviceInventory};
        use std::path::PathBuf;
        let gib = 1024 * 1024 * 1024;
        NodeDeviceInventory {
            node_id: NodeId(node),
            devices: vec![
                DeviceEntry {
                    mount_path: PathBuf::from(format!("/mnt/nvme{node}")),
                    media_class: MediaType::Nvme,
                    total_bytes: fast_gib * gib,
                    free_bytes: fast_gib * gib,
                    tag: None,
                    exclusive: true,
                },
                DeviceEntry {
                    mount_path: PathBuf::from(format!("/mnt/sata{node}")),
                    media_class: MediaType::Ssd,
                    total_bytes: slow_gib * gib,
                    free_bytes: slow_gib * gib,
                    tag: None,
                    exclusive: true,
                },
            ],
            refreshed_ms: 1_700_000_000_000,
        }
    }

    #[test]
    fn upsert_node_inventory_populates_catalog_and_bumps_inventory_clock() {
        let mut sm = StateMachineInner::new();
        let before_policy_ms = sm.catalog.policy_change_ms;
        let resp = sm.apply_command(&ControlCommand::UpsertNodeInventory {
            node_id: NodeId(7),
            inventory: inventory(7, 1500, 8000),
        });
        assert_eq!(resp, ControlResponse::Applied);
        assert_eq!(sm.catalog.inventories.len(), 1);
        assert!(sm.catalog.inventories.contains_key(&NodeId(7)));
        // policy_change_ms (the quiescence clock — N-5 fix) MUST NOT
        // be touched by inventory upserts; inventory_change_ms (D10)
        // MUST be bumped.
        assert_eq!(
            sm.catalog.policy_change_ms, before_policy_ms,
            "UpsertNodeInventory must not touch the quiescence clock"
        );
        assert!(sm.catalog.inventory_change_ms > 0);
    }

    #[test]
    fn upsert_node_inventory_is_idempotent_on_identical_inputs() {
        // I-DI6 (refresh idempotency): re-publishing identical
        // inventory MUST NOT touch inventory_change_ms either.
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::UpsertNodeInventory {
            node_id: NodeId(7),
            inventory: inventory(7, 1500, 8000),
        });
        let after_first = sm.catalog.inventory_change_ms;
        // Sleep a millisecond so a clock-bump would be visible.
        std::thread::sleep(std::time::Duration::from_millis(2));
        sm.apply_command(&ControlCommand::UpsertNodeInventory {
            node_id: NodeId(7),
            inventory: inventory(7, 1500, 8000),
        });
        assert_eq!(
            sm.catalog.inventory_change_ms, after_first,
            "identical re-publish must be a no-op (I-DI6)"
        );
        assert_eq!(sm.catalog.inventories.len(), 1);
    }

    #[test]
    fn set_placement_policy_bumps_revision_and_quiescence_clock() {
        // The initial catalog policy is `PlacementPolicy::built_in_default()`
        // (auto-populated via `ClusterDeviceCatalog::default()`), so setting
        // built_in_default is a no-op (idempotent). To force a real change
        // we use a custom single-tier policy.
        use kiseki_common::{DeviceMatcher, MediaType, PolicyMode, TierCapacity, TierPolicy};
        let mut sm = StateMachineInner::new();
        let before_rev = sm.catalog.policy_revision;
        let custom_policy = kiseki_common::PlacementPolicy {
            tiers: vec![TierPolicy {
                tier: kiseki_common::FjallStoreTier::SmallObject,
                preferences: vec![DeviceMatcher::Class(MediaType::Ssd)],
                mode: PolicyMode::BestEffort,
                capacity: TierCapacity::Auto {
                    target_pct: 50,
                    floor_bytes: 1024 * 1024 * 1024,
                    ceiling_bytes: None,
                },
            }],
        };
        let resp = sm.apply_command(&ControlCommand::SetPlacementPolicy {
            policy: custom_policy,
        });
        assert_eq!(resp, ControlResponse::Applied);
        assert_eq!(sm.catalog.policy_revision, before_rev + 1);
        assert!(sm.catalog.policy_change_ms > 0);
    }

    #[test]
    fn set_workload_params_bumps_revision_and_quiescence_clock() {
        let mut sm = StateMachineInner::new();
        let before_rev = sm.catalog.policy_revision;
        let params = kiseki_common::WorkloadParams {
            avg_file_bytes: 64 * 1024,
            ..kiseki_common::WorkloadParams::default()
        };
        let resp = sm.apply_command(&ControlCommand::SetWorkloadParams { params });
        assert_eq!(resp, ControlResponse::Applied);
        assert_eq!(sm.catalog.policy_revision, before_rev + 1);
        assert_eq!(sm.catalog.workload.avg_file_bytes, 64 * 1024);
        assert!(sm.catalog.policy_change_ms > 0);
    }

    #[test]
    fn idempotent_policy_set_does_not_bump_revision() {
        // Replaying the same policy MUST NOT bump revision (I-DI6
        // applies broadly — idempotent applies don't churn the
        // catalog).
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::SetPlacementPolicy {
            policy: kiseki_common::PlacementPolicy::built_in_default(),
        });
        let after_first = sm.catalog.policy_revision;
        sm.apply_command(&ControlCommand::SetPlacementPolicy {
            policy: kiseki_common::PlacementPolicy::built_in_default(),
        });
        assert_eq!(sm.catalog.policy_revision, after_first);
    }

    #[test]
    fn catalog_round_trips_through_full_snapshot_install_path() {
        // ADR-049 phase 1 wire test: snapshot includes catalog,
        // install repopulates it. Q36 acceptance: future field
        // additions are forward-compatible via #[serde(default)].
        //
        // Note: initial catalog policy is `built_in_default` (auto via
        // `ClusterDeviceCatalog::default()`), so setting `built_in_default`
        // would be a no-op. We use a tiny custom policy that *differs*
        // from the default so the revision bump is observable.
        use kiseki_common::{DeviceMatcher, MediaType, PolicyMode, TierCapacity, TierPolicy};
        let mut sm = StateMachineInner::new();
        sm.apply_command(&ControlCommand::CreateNamespace {
            namespace_id: ns(),
            tenant_id: org(),
            shards: vec![full_range(shard(1))],
        });
        sm.apply_command(&ControlCommand::UpsertNodeInventory {
            node_id: NodeId(7),
            inventory: inventory(7, 1500, 8000),
        });
        let custom_policy = kiseki_common::PlacementPolicy {
            tiers: vec![TierPolicy {
                tier: kiseki_common::FjallStoreTier::SmallObject,
                preferences: vec![DeviceMatcher::Class(MediaType::Ssd)],
                mode: PolicyMode::BestEffort,
                capacity: TierCapacity::Auto {
                    target_pct: 50,
                    floor_bytes: 1024 * 1024 * 1024,
                    ceiling_bytes: None,
                },
            }],
        };
        sm.apply_command(&ControlCommand::SetPlacementPolicy {
            policy: custom_policy,
        });
        let snap = ControlSnapshot {
            namespaces: sm.namespaces.clone(),
            catalog: sm.catalog.clone(),
        };
        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let parsed: ControlSnapshot = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(parsed.namespaces.len(), 1);
        assert_eq!(parsed.catalog.inventories.len(), 1);
        assert!(parsed.catalog.inventories.contains_key(&NodeId(7)));
        assert_eq!(parsed.catalog.policy_revision, 1);
        assert_eq!(parsed.catalog.policy.tiers.len(), 1);
    }

    #[test]
    fn set_placement_policy_rejected_at_apply_when_absolute_overcommits() {
        // I-DI9 + DI-5 (unit-test seed): an operator pushing
        // `Absolute { 100 TiB }` SmallObject against a cluster with
        // F_total = 9 TiB must be rejected at apply time. The
        // catalog state stays unchanged so admin RPC + operator see
        // a clean "policy not adopted" surface.
        use kiseki_common::{DeviceMatcher, MediaType, PolicyMode, TierCapacity, TierPolicy};
        const TIB: u64 = 1024 * 1024 * 1024 * 1024;
        const GIB: u64 = 1024 * 1024 * 1024;
        let mut sm = StateMachineInner::new();
        // Seed inventories: 6 × 1.5 TiB NVMe.
        for n in 1u64..=6 {
            sm.apply_command(&ControlCommand::UpsertNodeInventory {
                node_id: NodeId(n),
                inventory: inventory(n, 1500, 0),
            });
        }
        let before_revision = sm.catalog.policy_revision;
        let policy = kiseki_common::PlacementPolicy {
            tiers: vec![TierPolicy {
                tier: kiseki_common::FjallStoreTier::SmallObject,
                preferences: vec![DeviceMatcher::Class(MediaType::Nvme)],
                mode: PolicyMode::BestEffort,
                capacity: TierCapacity::Absolute {
                    cluster_bytes: 100 * TIB,
                },
            }],
        };
        let resp = sm.apply_command(&ControlCommand::SetPlacementPolicy { policy });
        match resp {
            ControlResponse::PolicyRejected { reason } => {
                // The reason should mention I-DI8 violation and the
                // demand/available values for operator clarity.
                assert!(
                    reason.contains("I-DI8") || reason.contains("exceeds available"),
                    "expected reject reason to mention I-DI8 / exceeds available, got: {reason}",
                );
            }
            other => panic!("expected PolicyRejected, got {other:?}"),
        }
        assert_eq!(
            sm.catalog.policy_revision, before_revision,
            "I-DI9: rejected SetPlacementPolicy must NOT bump policy_revision"
        );
        // Cluster still has F_total > 0 so the rejection is real.
        assert!(sm.catalog.f_total() > 0);
        let _ = GIB; // silence unused
    }

    #[test]
    fn pre_upgrade_snapshot_decodes_with_serde_default_catalog() {
        // Q36 acceptance: a pre-upgrade snapshot (no `catalog`
        // field in the JSON) MUST decode cleanly via
        // #[serde(default)] -> ClusterDeviceCatalog::default().
        //
        // After ADR-049 phase 5a continued, `ClusterDeviceCatalog::default()`
        // populates `policy` from `PlacementPolicy::built_in_default()` so
        // a fresh cluster has sensible defaults without an operator
        // `SetPlacementPolicy` apply. The pre-upgrade decode therefore
        // yields the full default policy, NOT an empty one.
        let pre_upgrade_json = r#"{"namespaces": {}}"#;
        let parsed: ControlSnapshot =
            serde_json::from_str(pre_upgrade_json).expect("forward-compat decode");
        assert!(parsed.namespaces.is_empty());
        assert!(parsed.catalog.inventories.is_empty());
        assert_eq!(parsed.catalog.policy_revision, 0);
        // Default policy covers every catalog-resolved tier.
        for tier in kiseki_common::FjallStoreTier::catalog_resolved() {
            assert!(
                parsed.catalog.policy.for_tier(tier).is_some(),
                "default policy must cover {tier:?}"
            );
        }
    }
}
