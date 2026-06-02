//! `OpenRaftControlStore` — wires the control-plane state machine to
//! an openraft handle on the multiplexed Raft transport (ADR-041).
//!
//! ## Apply-side fan-out (`ApplyHook`)
//!
//! The state machine itself is Raft-agnostic and free of side
//! effects. The apply hook is what makes ADR-033 §4 different from
//! "just persist the map" — every node's apply runs the same hook,
//! which lets a single consensus event drive cluster-wide changes
//! to the per-shard Raft groups (e.g. creating the new shard's
//! group locally on every replica during a split).
//!
//! The hook is a trait the runtime injects at construction time so
//! `kiseki-server::cluster_control` doesn't directly depend on
//! `kiseki-log` — the runtime hands it an adapter that owns an
//! `Arc<RaftShardStore>`.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_common::ids::{NodeId, OrgId, ShardId};
use kiseki_raft::{
    FjallRaftLogStore, KisekiNode, KisekiRaftConfig, MemLogStore, RegistryHandle, TcpNetworkFactory,
};
use openraft::log_id::RaftLogId;
use openraft::type_config::async_runtime::WatchReceiver;
use openraft::Raft;

use super::commands::ControlCommand;
use super::state_machine::ControlStateMachine;
use super::types::{ControlResponse, ControlTypeConfig};

#[allow(unused_imports)] // re-exported by mod.rs
pub use super::types::CONTROL_RAFT_GROUP_ID;

type C = ControlTypeConfig;

/// Hook invoked on every node when the control-plane state machine
/// applies a topology mutation. The runtime supplies an
/// implementation that bridges to `RaftShardStore`:
///
/// * `on_split` — locally creates the new per-shard Raft group on
///   this node (`bootstrap = false` for non-leader nodes; the leader
///   also drives the source-side rebalancing through the source
///   shard's existing Raft group).
/// * `on_retire` — locally drops the retired per-shard Raft group
///   after its deltas have been drained.
/// * `on_create_namespace` — locally creates each initial shard's
///   per-shard Raft group.
///
/// All methods run after the control-plane log entry commits — i.e.
/// every method observes a deterministic, replicated view of the
/// shard map.
pub trait ApplyHook: Send + Sync + 'static {
    /// Called once per shard listed in `CreateNamespace`. Each node
    /// locally creates the shard's Raft group; node 1 (bootstrap)
    /// initializes it.
    fn on_create_namespace(
        &self,
        namespace_id: &str,
        tenant_id: OrgId,
        shard_id: ShardId,
        leader_node: NodeId,
    );

    /// Called on every node after `RecordSplit` commits. The node
    /// locally creates the new shard's Raft group with the same id;
    /// the multiplexed listener picks it up automatically.
    fn on_split(
        &self,
        namespace_id: &str,
        source_shard_id: ShardId,
        new_shard_id: ShardId,
        new_leader: NodeId,
    );

    /// Called on every node after `RecordMerge` commits. Hook for
    /// quiescing the retired shard's Raft group — the actual
    /// removal happens on `RetireShard` after deltas drain.
    fn on_merge(&self, namespace_id: &str, surviving_shard_id: ShardId, retired_shard_id: ShardId);

    /// Called on every node after `RetireShard` commits. The node
    /// drops the per-shard Raft group locally.
    fn on_retire(&self, namespace_id: &str, shard_id: ShardId);
}

/// No-op apply hook — used by unit tests and single-node setups
/// that don't need cluster-wide fan-out into per-shard Raft groups.
#[allow(dead_code)] // production runtime always wires `ShardStoreApplyHook`
pub struct NoopApplyHook;

impl ApplyHook for NoopApplyHook {
    fn on_create_namespace(&self, _: &str, _: OrgId, _: ShardId, _: NodeId) {}
    fn on_split(&self, _: &str, _: ShardId, _: ShardId, _: NodeId) {}
    fn on_merge(&self, _: &str, _: ShardId, _: ShardId) {}
    fn on_retire(&self, _: &str, _: ShardId) {}
}

/// Apply hook that bridges control-plane mutations to the per-node
/// `RaftShardStore`. Every node runs this hook on every applied
/// command, so a single consensus event drives cluster-wide
/// registration (and eventual retirement) of per-shard Raft groups.
///
/// Idempotent: every method short-circuits when the local
/// `RaftShardStore` already (or no longer) hosts the shard, so
/// replay through snapshot install + tail catch-up is safe.
///
/// **Membership initialization is NOT done here.** `create_shard`
/// only registers the per-shard Raft handle — it never calls
/// `Raft::initialize()`. The admin RPC handler that submitted the
/// command (`storage_admin::split_shard`) calls
/// `RaftShardStore::initialize_shard` separately on the new
/// leader node, AFTER the control-plane submit returns (which
/// guarantees commit on a majority). Decoupling the two phases
/// is what avoids the deadlock observed when leader
/// `Raft::initialize()` was inline with `create_shard`.
pub struct ShardStoreApplyHook {
    raft_store: Arc<kiseki_log::RaftShardStore>,
    /// This node's id — recorded for tracing only; the apply hook
    /// no longer differentiates leader vs follower behavior.
    self_node_id: u64,
    /// Default shard config for new shards. `RecordSplit` inherits
    /// the source's config implicitly via the per-shard Raft state
    /// machine; this default is for `CreateNamespace` apply.
    config: kiseki_log::ShardConfig,
    /// Optional composition-hydrator registry. When set, every
    /// `on_create_namespace` / `on_split` also registers the new
    /// shard with the registry so a per-shard hydrator task spawns
    /// alongside the per-shard Raft group. Without this the
    /// follower's composition store never installs Create deltas
    /// for non-bootstrap shards (the original Phase 16f wiring was
    /// single-shard); see `multi-node-raft.feature:310`.
    ///
    /// `OnceLock` (init-once, then read-only) because the registry
    /// is attached after the gateway's `CompositionStore` is built
    /// — which happens later in `runtime::run` than the apply hook
    /// itself. Pre-attach, `on_create_namespace` simply skips the
    /// registry step (no shards are committed before boot reaches
    /// the attach point in any normal flow).
    hydrator_registry: std::sync::OnceLock<Arc<kiseki_composition::HydratorRegistry>>,
    /// Raft runtime handle. GH #101: when this node is the assigned
    /// `leader_node` for a freshly-created shard, the apply hook
    /// spawns `initialize_shard` for it onto this runtime so per-shard
    /// leadership distributes across nodes (rather than the control-
    /// plane leader leading every shard). Spawned, never inline —
    /// inline `Raft::initialize()` inside `create_shard` deadlocks the
    /// apply pipeline (see the struct doc above).
    raft_runtime: tokio::runtime::Handle,
}

impl ShardStoreApplyHook {
    /// Build a hook bound to the given `RaftShardStore`. The
    /// composition-hydrator registry is attached separately via
    /// [`Self::attach_hydrator_registry`] because the registry
    /// needs the gateway's `CompositionStore` handle which isn't
    /// constructed until later in `runtime::run`.
    #[must_use]
    pub fn new(
        raft_store: Arc<kiseki_log::RaftShardStore>,
        self_node_id: u64,
        raft_runtime: tokio::runtime::Handle,
    ) -> Self {
        Self {
            raft_store,
            self_node_id,
            config: kiseki_log::ShardConfig::default(),
            hydrator_registry: std::sync::OnceLock::new(),
            raft_runtime,
        }
    }

    /// GH #101: the assigned `leader_node` initializes a fresh shard's
    /// Raft membership so leadership distributes across nodes. Spawned
    /// onto the Raft runtime (never inline — inline init deadlocks the
    /// apply pipeline) with a bounded retry that absorbs the window
    /// where peer replicas are still registering their local group.
    /// Idempotent: a re-fired apply (snapshot install + tail catch-up)
    /// re-spawns, but `initialize_membership` maps the already-
    /// initialized `NotAllowed` to `Ok`.
    fn spawn_initialize_as_leader(&self, shard_id: ShardId) {
        let store = Arc::clone(&self.raft_store);
        self.raft_runtime.spawn(async move {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                match store.initialize_shard_async(shard_id).await {
                    Ok(()) => {
                        tracing::info!(
                            shard_id = %shard_id.0,
                            "apply hook: initialized per-shard Raft membership as assigned leader (GH #101)",
                        );
                        return;
                    }
                    Err(e) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        let _ = e;
                    }
                    Err(e) => {
                        tracing::warn!(
                            shard_id = %shard_id.0,
                            error = %e,
                            "apply hook: initialize_shard failed after 15s — shard will not accept writes",
                        );
                        return;
                    }
                }
            }
        });
    }

    /// Attach the composition-hydrator registry to an existing
    /// hook. Once attached, every shard the apply hook locally
    /// registers (on create or split) also gets a per-shard
    /// hydrator task. Idempotent — calling twice keeps the first
    /// registry.
    pub fn attach_hydrator_registry(&self, registry: Arc<kiseki_composition::HydratorRegistry>) {
        let _ = self.hydrator_registry.set(registry);
    }

    fn create_shard_idempotent(&self, shard_id: ShardId, tenant_id: OrgId) {
        if self.raft_store.has_shard(shard_id) {
            tracing::debug!(
                shard_id = %shard_id.0,
                "control-plane apply hook: shard already exists locally — skipping create",
            );
            // Still register with the hydrator registry even if the
            // Raft group already exists locally: on replay through
            // snapshot install + tail catch-up the apply hook re-
            // fires, and we want every known shard to have a poll
            // loop. `register` is itself idempotent.
            if let Some(registry) = self.hydrator_registry.get() {
                registry.register(shard_id);
            }
            return;
        }
        // `raft_addr=None` because the listener is already running
        // (the runtime called `ensure_listener_started` before this
        // store was constructed); subsequent shards register via the
        // already-cached `RegistryHandle` inside RaftShardStore.
        // No `bootstrap` flag — `create_shard` no longer initializes
        // membership inline. The admin RPC handler runs
        // `initialize_shard` after submit returns.
        self.raft_store.create_shard(
            shard_id,
            tenant_id,
            kiseki_common::ids::NodeId(self.self_node_id),
            self.config.clone(),
            None,
        );
        // The hydrator registry must learn about every per-shard
        // Raft group this node hosts. Without this the composition
        // hydrator only polls the bootstrap shard and Create deltas
        // on every other shard go unapplied on followers (the bug
        // root-caused 2026-05-18).
        if let Some(registry) = self.hydrator_registry.get() {
            registry.register(shard_id);
        }
        tracing::info!(
            shard_id = %shard_id.0,
            tenant_id = %tenant_id.0,
            self_node_id = self.self_node_id,
            "control-plane apply hook: registered per-shard Raft group locally",
        );
    }
}

impl ApplyHook for ShardStoreApplyHook {
    fn on_create_namespace(
        &self,
        _namespace_id: &str,
        tenant_id: OrgId,
        shard_id: ShardId,
        leader_node: NodeId,
    ) {
        self.create_shard_idempotent(shard_id, tenant_id);
        // GH #101: every node registers the group locally above; the
        // assigned `leader_node` additionally initializes membership so
        // it becomes this shard's Raft leader. Distributing the
        // initialize call across each shard's assigned leader is what
        // fans leadership out across the cluster — the previous
        // centralized path (control-plane leader initializes all of a
        // namespace's shards) left every leader on one node. `on_split`
        // deliberately does NOT do this: `StorageAdminService::split_shard`
        // initializes the new shard explicitly on its new leader, so
        // adding it here would double-init the same group.
        if leader_node.0 == self.self_node_id {
            self.spawn_initialize_as_leader(shard_id);
        }
    }

    fn on_split(
        &self,
        _namespace_id: &str,
        source_shard_id: ShardId,
        new_shard_id: ShardId,
        _new_leader: NodeId,
    ) {
        // Tenant inherited from source shard.
        let Some(tenant_id) = self.raft_store.shard_tenant(source_shard_id) else {
            tracing::warn!(
                source = %source_shard_id.0,
                new = %new_shard_id.0,
                "apply hook on_split: source shard tenant unknown locally — \
                 cluster-wide split fan-out skipped on this node",
            );
            return;
        };
        self.create_shard_idempotent(new_shard_id, tenant_id);
    }

    fn on_merge(
        &self,
        _namespace_id: &str,
        _surviving_shard_id: ShardId,
        _retired_shard_id: ShardId,
    ) {
        // Merge keeps both shards alive locally until the matching
        // RetireShard command lands. Quiescence (read-only,
        // refuse new appends) lives in `LogOps::merge_shards`'s
        // existing source-side flow.
    }

    fn on_retire(&self, _namespace_id: &str, _shard_id: ShardId) {
        // Local removal of the per-shard Raft group is a follow-up
        // — the openraft handle owns background tasks that need a
        // graceful shutdown sequence. For now the retired shard
        // stays resident on every node but is marked `Retiring` in
        // the control-plane state machine, so routing skips it.
        // Resource cleanup tracked in ADR-034.
    }
}

/// Cluster-wide control-plane Raft store.
///
/// Owns one `Raft<ControlTypeConfig>` handle plus the state machine
/// `Arc`. Construction registers the group with the supplied
/// `RegistryHandle` (the multiplexed listener) so cross-node RPCs
/// for `CONTROL_RAFT_GROUP_ID` route in.
pub struct OpenRaftControlStore {
    raft: Raft<C, ControlStateMachine>,
    /// State machine handle — exposed via `state()` so future
    /// read-side admin RPCs can read the namespace shard map
    /// without going through Raft.
    #[allow(dead_code)] // exposed by `state()` for follow-up reads
    state: ControlStateMachine,
    /// Optional metrics — `None` for tests, `Some` in production.
    /// Wired by `with_metrics(...)` after construction.
    metrics: Option<Arc<super::ClusterControlMetrics>>,
}

impl OpenRaftControlStore {
    /// Build the control-plane Raft group and register it with the
    /// multiplexed listener.
    ///
    /// * `peers` — full peer list (including this node). Always
    ///   uses the multiplexed TCP transport (ADR-041); the runtime
    ///   only constructs this store in multi-node mode (gate:
    ///   `cfg.raft_peers.len() > 1`), so there is no single-node
    ///   stub-network branch.
    /// * `data_dir` — when `Some`, the Raft log is persisted to
    ///   `<dir>/raft/cluster_control/` so the control-plane
    ///   group's membership and committed entries survive node
    ///   restart. The BDD harness's `@leader-change` scenarios
    ///   stress this: every node cycles through kill+restart
    ///   during a single test session, and without persistence the
    ///   restarted nodes lose all knowledge of the cluster's
    ///   leader / membership and the group eventually wedges with
    ///   `forward request to: None, None`. With the persistent log
    ///   backing, the restarted node rejoins via `AppendEntries`
    ///   from the surviving leader.
    /// * `registry` — handle to the shared multiplexed listener.
    /// * `apply_hook` — runtime-supplied bridge to per-shard Raft
    ///   groups (see `ApplyHook` doc above).
    /// * `bootstrap` — true on the seed node. Must be `true` on
    ///   exactly one node at first cluster boot; false on every
    ///   subsequent restart and on every other node.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        node_id: u64,
        peers: &BTreeMap<u64, String>,
        data_dir: Option<&std::path::Path>,
        registry: &RegistryHandle,
        apply_hook: Arc<dyn ApplyHook>,
        bootstrap: bool,
        metrics: Option<Arc<super::ClusterControlMetrics>>,
        shard_map: Option<Arc<kiseki_control::shard_topology::NamespaceShardMapStore>>,
    ) -> Result<Self, std::io::Error> {
        let config = KisekiRaftConfig::default_config();
        // State machine owns the apply hook — fires from inside
        // `RaftStateMachine::apply` on every node so the cluster-
        // wide fan-out happens deterministically with consensus.
        // Metrics are attached BEFORE `Raft::new` so the clone
        // openraft retains internally sees the same metrics
        // handle (the engine's apply runs on its own clone, not
        // on the one we hold via `self.state`).
        let mut state_machine = ControlStateMachine::with_apply_hook(apply_hook);
        if let Some(m) = metrics.as_ref() {
            state_machine = state_machine.with_apply_metrics(Arc::clone(m));
        }
        // ADR-033 §5: hydrate the gateway-readable shard map on every
        // apply. Without this binding the gateway falls back to the
        // namespace's single primary `comp.shard_id` for every write,
        // and multi-shard fanout is dead code.
        if let Some(sm) = shard_map.as_ref() {
            state_machine = state_machine.with_shard_map(Arc::clone(sm));
        }

        let members: BTreeMap<u64, KisekiNode> = peers
            .iter()
            .map(|(id, addr)| (*id, KisekiNode::new(addr)))
            .collect();

        let network = TcpNetworkFactory::<C>::new(CONTROL_RAFT_GROUP_ID);

        // Persistent log store when `data_dir` is set; otherwise
        // in-memory. `already_initialized` lets us skip
        // `Raft::initialize` on a persistent-backed restart even if
        // the caller still passes `bootstrap = true`.
        let (raft, already_initialized) = if let Some(dir) = data_dir {
            let raft_dir = dir.join("raft");
            std::fs::create_dir_all(&raft_dir).ok();
            let log_path = raft_dir.join("cluster_control");
            let log_store = FjallRaftLogStore::<C>::open(&log_path).map_err(|e| {
                std::io::Error::other(format!(
                    "control raft log store {}: {e}",
                    log_path.display()
                ))
            })?;
            let has_state = log_store.has_state();
            let raft = Raft::new(node_id, config, network, log_store, state_machine.clone())
                .await
                .map_err(|e| std::io::Error::other(format!("control raft init: {e}")))?;
            (raft, has_state)
        } else {
            let log_store = MemLogStore::<C>::new();
            let raft = Raft::new(node_id, config, network, log_store, state_machine.clone())
                .await
                .map_err(|e| std::io::Error::other(format!("control raft init: {e}")))?;
            (raft, false)
        };

        // Register with the multiplexed listener BEFORE initialize()
        // — otherwise the leader's first AppendEntries on followers
        // races against group registration and the follower returns
        // unknown_shard.
        registry.register_shard::<C, ControlStateMachine>(
            CONTROL_RAFT_GROUP_ID,
            Arc::new(raft.clone()),
        );

        if bootstrap && !already_initialized {
            // First boot of a fresh cluster: write the initial
            // membership entry. Persistent restarts (`already_initialized`)
            // skip this — the persistent log already holds the membership
            // and openraft would reject a repeat call with
            // `not allowed`.
            if let Err(e) = raft.initialize(members).await {
                tracing::debug!(
                    error = %e,
                    "control-plane raft initialize() returned error \
                     (probably already initialized — proceeding)",
                );
            }
        }

        Ok(Self {
            raft,
            state: state_machine,
            metrics,
        })
    }

    /// Borrow the underlying state machine — for tests and read-side
    /// admin RPCs that want to inspect the namespace map without
    /// going through Raft.
    #[must_use]
    #[allow(dead_code)] // wired when read-side admin RPCs land
    pub fn state(&self) -> ControlStateMachine {
        self.state.clone()
    }

    /// Borrow the underlying Raft handle.
    #[must_use]
    #[allow(dead_code)] // exposed for tests
    pub fn raft(&self) -> &Raft<C, ControlStateMachine> {
        &self.raft
    }

    /// Submit a `ControlCommand` through Raft. Returns the typed
    /// `ControlResponse` once consensus commits the entry.
    ///
    /// Apply hooks fire from inside `RaftStateMachine::apply` on
    /// every node — the leader's apply runs first (commit ⇒ apply
    /// before this `await` returns), followers' applies catch up
    /// in lockstep. Idempotency on `ApplyHook::on_*` keeps the
    /// race safe.
    #[tracing::instrument(skip(self), fields(cmd = %cmd, op = super::ClusterControlMetrics::op_label(&cmd)))]
    pub async fn submit(&self, cmd: ControlCommand) -> Result<ControlResponse, std::io::Error> {
        let op = super::ClusterControlMetrics::op_label(&cmd);
        let started = std::time::Instant::now();
        let result = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|e| std::io::Error::other(format!("control client_write: {e}")));
        let elapsed = started.elapsed();
        if let Some(m) = self.metrics.as_ref() {
            let outcome = match &result {
                Ok(_) => super::metrics::outcome::OK,
                // openraft's `ForwardToLeader` error variant
                // surfaces as a string with `forward request to:`
                // — match heuristically so the `forward` outcome
                // tracks election windows separately from generic
                // errors.
                Err(e) if e.to_string().contains("forward request to") => {
                    super::metrics::outcome::FORWARD
                }
                Err(_) => super::metrics::outcome::ERROR,
            };
            m.record_submit(op, outcome, elapsed);
        }
        result.map(|r| r.response().clone())
    }

    /// Submit `cmd` and wait until every voter's `matched` log index
    /// reaches the committed entry — so the apply hook has fired on
    /// every voter before this returns.
    ///
    /// Without this barrier, callers that depend on side effects of
    /// the apply hook (e.g. `ShardStoreApplyHook::on_create_namespace`
    /// creating per-shard Raft groups on followers, or
    /// `ControlStateMachine::hydrate_shard_map` populating the
    /// gateway-readable shard map) race against follower replication:
    /// `submit` returns once the LEADER applies, but followers'
    /// applies happen asynchronously over `AppendEntries`. The 2026-05-18
    /// slow-tier RCA pinned this race to 6-node EC GET-from-followers
    /// returning 404 immediately after a bootstrap-time
    /// `CreateNamespace`.
    ///
    /// `matched` on the leader's replication metric is the highest log
    /// index a follower has confirmed *receiving* (not strictly
    /// applying). For our case it's a good-enough proxy — openraft's
    /// apply task drains the log monotonically and apply latency on
    /// the follower side is sub-millisecond once an entry lands. A
    /// tiny tail sleep after the matched check absorbs that gap.
    pub async fn submit_and_wait_for_voters(
        &self,
        cmd: ControlCommand,
        deadline: std::time::Duration,
    ) -> Result<ControlResponse, std::io::Error> {
        let op = super::ClusterControlMetrics::op_label(&cmd);
        let started = std::time::Instant::now();
        let result = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|e| std::io::Error::other(format!("control client_write: {e}")));
        let resp = match result {
            Ok(r) => r,
            Err(e) => {
                if let Some(m) = self.metrics.as_ref() {
                    let outcome = if e.to_string().contains("forward request to") {
                        super::metrics::outcome::FORWARD
                    } else {
                        super::metrics::outcome::ERROR
                    };
                    m.record_submit(op, outcome, started.elapsed());
                }
                return Err(e);
            }
        };
        let target_index = RaftLogId::index(resp.log_id());
        self.wait_for_voters_applied(target_index, deadline).await?;
        if let Some(m) = self.metrics.as_ref() {
            m.record_submit(op, super::metrics::outcome::OK, started.elapsed());
        }
        Ok(resp.response().clone())
    }

    /// Poll openraft metrics until every replication peer's `matched`
    /// log index reaches `target_index`. Returns Ok on success or a
    /// timeout error after `deadline`.
    ///
    /// On a non-leader the leader's per-follower replication state
    /// isn't observable from this node — fall back to a leader-local
    /// applied-index check.
    pub async fn wait_for_voters_applied(
        &self,
        target_index: u64,
        deadline: std::time::Duration,
    ) -> Result<(), std::io::Error> {
        let stop = std::time::Instant::now() + deadline;
        loop {
            let m = self.raft.metrics().borrow_watched().clone();
            let leader_applied = m.last_applied.as_ref().map_or(0, RaftLogId::index);
            if leader_applied < target_index {
                if std::time::Instant::now() > stop {
                    return Err(std::io::Error::other(format!(
                        "control-plane leader did not apply through {target_index} within {deadline:?}",
                    )));
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                continue;
            }
            // `replication` is `Some` only on the leader. On a non-
            // leader we can't observe follower state — `last_applied`
            // above is the best signal we have, so return.
            let Some(repl) = m.replication.as_ref() else {
                return Ok(());
            };
            let all_caught_up = repl.iter().all(|(_, opt_log_id)| {
                opt_log_id.as_ref().map_or(0, RaftLogId::index) >= target_index
            });
            if all_caught_up {
                // Small tail sleep so the follower's apply task drains
                // the in-flight entry before the caller proceeds.
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                return Ok(());
            }
            if std::time::Instant::now() > stop {
                return Err(std::io::Error::other(format!(
                    "control-plane voters did not match through {target_index} within {deadline:?}",
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    /// Dispatch a control-plane apply hook for a single command.
    /// Called from the state machine's `apply` method on every
    /// node (not just the leader) so cluster-wide side effects
    /// happen deterministically with consensus.
    pub(crate) fn dispatch_hook(hook: &dyn ApplyHook, cmd: &ControlCommand) {
        match cmd {
            ControlCommand::CreateNamespace {
                namespace_id,
                tenant_id,
                shards,
            } => {
                for s in shards {
                    hook.on_create_namespace(namespace_id, *tenant_id, s.shard_id, s.leader_node);
                }
            }
            ControlCommand::RecordSplit {
                namespace_id,
                source_shard_id,
                new_shard_id,
                new_leader,
                ..
            } => {
                hook.on_split(namespace_id, *source_shard_id, *new_shard_id, *new_leader);
            }
            ControlCommand::RecordMerge {
                namespace_id,
                surviving_shard_id,
                retired_shard_id,
                ..
            } => {
                hook.on_merge(namespace_id, *surviving_shard_id, *retired_shard_id);
            }
            ControlCommand::RetireShard {
                namespace_id,
                shard_id,
            } => {
                hook.on_retire(namespace_id, *shard_id);
            }
            // ADR-049 catalog mutations don't drive per-node side
            // effects via the apply hook. The catalog read side
            // (`ControlStateMachine::catalog()`) is consumed
            // directly by the resolver + admin RPC at boot.
            ControlCommand::UpsertNodeInventory { .. }
            | ControlCommand::SetPlacementPolicy { .. }
            | ControlCommand::SetWorkloadParams { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiseki_common::ids::OrgId;
    use uuid::Uuid;

    /// Single-node smoke: create namespace via Raft, observe it in
    /// the state machine after submit returns.
    #[tokio::test]
    async fn single_node_create_namespace_round_trips() {
        let listener = kiseki_raft::tcp_transport::RaftRpcListener::new("127.0.0.1:0".into(), None);
        let registry = listener.registry();
        let mut peers = BTreeMap::new();
        peers.insert(1, "127.0.0.1:1".to_owned());
        let store = OpenRaftControlStore::new(
            1,
            &peers,
            None,
            &registry,
            Arc::new(NoopApplyHook),
            true,
            None,
            None,
        )
        .await
        .expect("control store init");
        let ns_id = "ns-test".to_owned();
        let cmd = ControlCommand::CreateNamespace {
            namespace_id: ns_id.clone(),
            tenant_id: OrgId(Uuid::from_u128(1)),
            shards: vec![super::super::commands::ShardRecord {
                shard_id: ShardId(Uuid::from_u128(1)),
                range_start: [0u8; 32],
                range_end: [0xFFu8; 32],
                leader_node: NodeId(1),
            }],
        };
        let resp = store.submit(cmd).await.expect("submit ok");
        assert!(matches!(
            resp,
            ControlResponse::NamespaceCreated { shard_count: 1 },
        ));
        let snap = store.state().snapshot().await;
        assert!(snap.namespaces.contains_key(&ns_id));
        assert_eq!(snap.namespaces[&ns_id].shards.len(), 1);
    }
}
