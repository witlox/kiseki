//! Raft-backed shard store for multi-node clusters.
//!
//! Wraps per-shard `OpenRaftLogStore` instances behind the async
//! `LogOps` trait (ADR-032). Each shard gets its own Raft group for
//! independent consensus. Methods are called directly from async
//! context — no sync↔async bridge needed.
//!
//! Phase I2: multi-node Raft consensus with in-memory Raft log
//! (`MemLogStore`). Durability via Raft replication to majority.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kiseki_common::ids::{ChunkId, NodeId, OrgId, SequenceNumber, ShardId};

use crate::delta::Delta;
use crate::error::LogError;
use crate::intent::{FjallIntentStore, InMemIntentStore, IntentStore, WriteIntent};
use crate::intent_sync::{build_intent_dispatcher, TransportIntentGatherer};
use crate::raft::state_machine::ClusterChunkStateEntry;
use crate::raft::OpenRaftLogStore;
use crate::raft_intent_sink::RaftLogIncorporationSink;
use crate::shard::{ShardConfig, ShardInfo, ShardState};
use crate::shard_committer::{PeerIntentGatherer, ShardCommitter};
use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps, ReadDeltasRequest};
use kiseki_common::locks::LockOrDie;

/// Default min durable copies for an acked decoupled write when
/// `KISEKI_MIN_ACKS` is unset — mirrors the chunk D-5 default (2-of-N).
const DEFAULT_MIN_ACKS: usize = 2;

/// Per-tick interval for a shard's committer supervisor (ADR-047 `LeaderSink`).
/// The supervisor polls leadership + drains (leader) or self-prunes (follower)
/// each tick; short enough that an idle shard's single pending intent is
/// incorporated within ~one hydrator cadence.
const COMMITTER_INTERVAL: Duration = Duration::from_millis(50);

/// Per-peer timeout for the `intent_put` fan — bounded like the chunk
/// fan-out so a single slow/dead peer cannot stall the producer's fast-ack.
const INTENT_FAN_PEER_TIMEOUT: Duration = Duration::from_secs(3);

/// A spawned per-shard committer task: its shutdown signal + join handle, so
/// [`RaftShardStore::shutdown`] / `Drop` can stop it cleanly. The loop runs on
/// a dedicated `std::thread` holding the Raft runtime handle (the
/// `raft_intent_sink` threading contract — never a tokio worker).
struct ShardCommitterHandle {
    shutdown: tokio::sync::watch::Sender<bool>,
    join: std::thread::JoinHandle<()>,
}

/// Holds an `Arc<OpenRaftLogStore>` so it can be passed BY VALUE as the
/// committer sink's [`IntentLogAppender`].
///
/// `RaftLogIncorporationSink::new` takes the appender by value, but the shard's
/// `OpenRaftLogStore` lives behind an `Arc` (shared with the `shards` map). The
/// inherent `IntentLogAppender for OpenRaftLogStore` impl in `raft_intent_sink`
/// can't be moved out of the `Arc`; this thin wrapper forwards through the
/// clone instead.
struct OpenRaftAppender {
    store: Arc<OpenRaftLogStore>,
}

impl crate::raft_intent_sink::IntentLogAppender for OpenRaftAppender {
    async fn append_intents(
        &self,
        items: Vec<crate::raft_store::IncorporateItem>,
    ) -> Result<(), LogError> {
        crate::raft_intent_sink::IntentLogAppender::append_intents(&*self.store, items).await
    }
}

/// Raft-backed shard store for multi-node clusters.
///
/// Holds a map of `ShardId → OpenRaftLogStore`. Each shard has its
/// own Raft group with independent leader election. The `LogOps`
/// trait methods are async (ADR-032), so callers await directly
/// without sync↔async bridging.
///
/// When `data_dir` is set, uses `FjallRaftLogStore` for persistent
/// Raft state (Phase 12b; ADR-022 rev-2). When `None`, uses
/// in-memory `MemLogStore`.
///
/// **ADR-041 multiplexed transport.** All shards on this node share
/// a single `RaftRpcListener`, lazily initialized on the first
/// `create_shard(... raft_addr=Some(addr) ...)` call. Subsequent
/// shards register their `Raft` handle with the same listener via
/// the cloned `RegistryHandle`. Pre-ADR-041, each shard tried to
/// `spawn_rpc_server` on its own — the second call hit `EADDRINUSE`
/// silently and that shard's cross-node messages never arrived.
pub struct RaftShardStore {
    shards: Mutex<HashMap<ShardId, Arc<OpenRaftLogStore>>>,
    /// Per-shard ADR-047 [`IntentStore`]s. Populated by `create_shard`
    /// alongside the Raft group so a peer can SERVE its intent state via the
    /// `IntentSync` aux dispatcher. Empty + inert in production this phase (no
    /// producer writes intents, no committer task queries) — the gatherer is
    /// only invoked by the 5c/5d committer task. Durable
    /// ([`FjallIntentStore`]) when `data_dir` is set, else
    /// [`InMemIntentStore`], mirroring the shard log store's persistence
    /// choice.
    intent_stores: Mutex<HashMap<ShardId, Arc<dyn IntentStore>>>,
    /// Shards whose per-shard [`IntentStore`] opened DURABLY (a
    /// [`FjallIntentStore`], not the [`InMemIntentStore`] degrade). The
    /// decoupled-ack path (`put_intent_and_fan` + the committer spawn) runs
    /// ONLY for a shard in this set — acking on a non-durable intent loses
    /// data on crash (the F-P5b-rpc-1 obligation). A shard with `data_dir =
    /// None` (in-memory test cluster) is NOT durable and so is absent here.
    durable_intent_shards: Mutex<HashSet<ShardId>>,
    /// Minimum durable copies (local + remote acks) for an acked decoupled
    /// write. From `KISEKI_MIN_ACKS`, else [`DEFAULT_MIN_ACKS`]. Mirrors the
    /// chunk D-5 quorum default.
    min_acks: usize,
    /// Per-shard async committer tasks spawned for each shard whose intent
    /// store opened durably (ADR-047 §F-P5b-rpc-1 — no committer on a
    /// non-durable shard). Stopped in [`Self::shutdown`] / `Drop`.
    committers: Mutex<HashMap<ShardId, ShardCommitterHandle>>,
    node_id: u64,
    peers: BTreeMap<u64, String>,
    /// Dedicated runtime for Raft async operations. Kept separate from
    /// the server's main runtime so NFS/FUSE threads can call `block_on`
    /// without nesting, and for Raft RPC server + bootstrap.
    rt: tokio::runtime::Runtime,
    data_dir: Option<PathBuf>,
    inline_store: Option<Arc<dyn kiseki_common::inline_store::InlineStore>>,
    /// Per-node Raft RPC listener registry handle. `None` until the
    /// first `create_shard` with `Some(raft_addr)` lazily binds the
    /// listener; from then on all shards on this node register here.
    listener_registry: Mutex<Option<kiseki_raft::tcp_transport::RegistryHandle>>,
    /// Optional Raft transport metrics. When set via
    /// `with_transport_metrics`, the lazy listener init wires them
    /// in via `RaftRpcListener::with_metrics(...)`.
    transport_metrics: Mutex<Option<Arc<kiseki_raft::transport_metrics::RaftTransportMetrics>>>,
    /// W12 (2026-06-02): per-shard intent-fan coalescer (producer-side).
    /// Spawned in `create_shard` for every shard whose intent store opened
    /// durably (the F-P5b-rpc-1 obligation). `put_intent_and_fan` routes
    /// through this so up to `KISEKI_INTENT_FAN_BATCH_MAX` PUTs share one
    /// local `put_batch` + one `intent_put` RPC per peer, amortising the
    /// fjall WAL sync and dropping the cluster-wide fan RPC volume by 4-8×.
    intent_fan_coalescers: IntentFanCoalescerMap,
    /// Lever 1 (2026-06-02): per-shard receiver-side `intent_put` fan
    /// coalescer. Spawned alongside the producer-side fan coalescer
    /// for every shard whose intent store opened durably. Mirrors the
    /// W12 producer-side design on the RECEIVER: aggregates incoming
    /// `intent_put` RPCs from multiple producers into one fjall
    /// `put_batch` per coalesce window, since receiver-side concurrency
    /// is naturally much higher than any one producer's per-shard
    /// concurrency.
    intent_recv_coalescers: IntentRecvCoalescerMap,
}

/// W12 (2026-06-02): per-shard handle map for the intent-fan coalescer.
/// Aliased so the field declaration on [`RaftShardStore`] stays inside the
/// `type_complexity` clippy budget.
type IntentFanCoalescerMap =
    Mutex<HashMap<ShardId, crate::intent_fan_coalescer::IntentFanCoalescer>>;

/// Lever 1 (2026-06-02): per-shard handle map for the receiver-side
/// `intent_put` coalescer. Aliased for the same reason as
/// [`IntentFanCoalescerMap`].
type IntentRecvCoalescerMap =
    Mutex<HashMap<ShardId, crate::intent_recv_coalescer::IntentRecvCoalescer>>;

impl RaftShardStore {
    /// Create a new (empty) Raft shard store.
    ///
    /// Spawns a dedicated tokio runtime for Raft async operations.
    /// When `data_dir` is `Some`, Raft log state is persisted to the
    /// fjall keyspace and survives restart. When `None`, uses
    /// in-memory log (volatile).
    ///
    /// ADR-047: decoupled-ack is THE write path for async-eligible surfaces
    /// (no capability gate). `create_shard` spawns a per-shard async
    /// committer for every shard whose intent store opens durably (the
    /// F-P5b-rpc-1 obligation — incorporating from a non-durable store
    /// could surface an intent a crash would lose). `put_intent_and_fan`
    /// performs the quorum intent-write. `min_acks` is read from
    /// `KISEKI_MIN_ACKS` (else [`DEFAULT_MIN_ACKS`]).
    #[must_use]
    pub fn new(node_id: u64, peers: BTreeMap<u64, String>, data_dir: Option<PathBuf>) -> Self {
        let min_acks = std::env::var("KISEKI_MIN_ACKS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|n| *n >= 1)
            .unwrap_or(DEFAULT_MIN_ACKS);
        // Build the Raft runtime on a background thread to avoid
        // "cannot start a runtime from within a runtime" when called
        // from an async context (e.g., run_main on the server's tokio runtime).
        // Default to half of available CPUs (min 4). Leaves the other
        // half for the S3/NFS gateway runtime, OS, and other processes.
        // Override with KISEKI_RAFT_THREADS for tuning.
        let raft_threads = std::env::var("KISEKI_RAFT_THREADS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| {
                std::thread::available_parallelism().map_or(8, |n| (n.get() / 2).max(4))
            });
        tracing::info!(threads = raft_threads, "Raft runtime");
        let rt = std::thread::spawn(move || {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(raft_threads)
                .thread_name("kiseki-raft")
                .enable_all()
                .build()
                .expect("failed to create Raft runtime")
        })
        .join()
        .expect("Raft runtime thread panicked");
        Self {
            shards: Mutex::new(HashMap::new()),
            intent_stores: Mutex::new(HashMap::new()),
            durable_intent_shards: Mutex::new(HashSet::new()),
            min_acks,
            committers: Mutex::new(HashMap::new()),
            node_id,
            peers,
            rt,
            data_dir,
            inline_store: None,
            listener_registry: Mutex::new(None),
            transport_metrics: Mutex::new(None),
            intent_fan_coalescers: Mutex::new(HashMap::new()),
            intent_recv_coalescers: Mutex::new(HashMap::new()),
        }
    }

    /// Attach the per-node Raft transport metrics. Must be called
    /// BEFORE the first `create_shard` with `Some(raft_addr)` —
    /// otherwise the lazy-init listener spawns without metrics.
    /// The runtime wires this from `KisekiMetrics::raft_transport`.
    pub fn set_transport_metrics(
        &self,
        metrics: Arc<kiseki_raft::transport_metrics::RaftTransportMetrics>,
    ) {
        let mut guard = self
            .transport_metrics
            .lock()
            .lock_or_die("raft_shard_store.transport_metrics");
        *guard = Some(metrics);
    }

    /// Set the inline store for small-file content (ADR-030).
    #[must_use]
    pub fn with_inline_store(
        mut self,
        store: Arc<dyn kiseki_common::inline_store::InlineStore>,
    ) -> Self {
        self.inline_store = Some(store);
        self
    }

    /// Ensure the multiplexed Raft listener (ADR-041) is running on
    /// `addr` and return a clone of its registry handle. Idempotent:
    /// the second call with a different `addr` is ignored — the
    /// existing listener (already-created on first call) wins.
    ///
    /// The runtime calls this BEFORE the first `create_shard` so the
    /// control-plane Raft group can register with the same listener
    /// as the per-shard groups (one port per node, ADR-041 §"Lifecycle").
    ///
    /// Returns the registry handle the runtime needs to call
    /// `register_shard` on for the control-plane group; the per-shard
    /// `create_shard` calls register through the same handle
    /// internally.
    pub fn ensure_listener_started(
        &self,
        addr: &str,
    ) -> kiseki_raft::tcp_transport::RegistryHandle {
        let mut guard = self
            .listener_registry
            .lock()
            .lock_or_die("raft_shard_store.listener_registry");
        if let Some(existing) = guard.as_ref() {
            return existing.clone();
        }
        let mut listener = kiseki_raft::tcp_transport::RaftRpcListener::new(addr.to_owned(), None);
        if let Some(m) = self
            .transport_metrics
            .lock()
            .lock_or_die("raft_shard_store.transport_metrics")
            .as_ref()
        {
            listener = listener.with_metrics(Arc::clone(m));
        }
        let reg = listener.registry();
        let handle = self.rt.handle().clone();
        handle.spawn(async move {
            if let Err(e) = listener.run().await {
                tracing::warn!(error = %e, "Raft RPC listener exited");
            }
        });
        tracing::info!(addr = %addr, "Raft RPC listener spawned (multiplexed, ADR-041)");
        *guard = Some(reg.clone());
        reg
    }

    /// Borrow the dedicated Raft tokio runtime handle. The
    /// `cluster_control` module needs this so it can run its own
    /// `Raft::new` / `client_write` futures on the same runtime as
    /// the per-shard Raft groups (mixing runtimes deadlocks tokio's
    /// reactor when openraft awaits cross-runtime).
    #[must_use]
    pub fn raft_runtime_handle(&self) -> tokio::runtime::Handle {
        self.rt.handle().clone()
    }

    /// Sync lookup of a shard's tenant. Returns `None` if the shard
    /// is unknown to this node. Used by `cluster_control::ApplyHook`
    /// (sync trait) to find the tenant of a source shard before
    /// locally creating the new (split-target) shard's Raft group on
    /// every replica.
    #[must_use]
    pub fn shard_tenant(&self, shard_id: ShardId) -> Option<OrgId> {
        let store = self.get_shard(shard_id).ok()?;
        self.shard_health_blocking(&store).ok().map(|i| i.tenant_id)
    }

    /// Sync availability check: does this node already host the
    /// shard's Raft group? Used by `cluster_control::ApplyHook` to
    /// stay idempotent — replaying `RecordSplit` on a node that
    /// already has the new shard must not double-create it.
    #[must_use]
    pub fn has_shard(&self, shard_id: ShardId) -> bool {
        self.shards
            .lock()
            .lock_or_die("raft_shard_store.shards")
            .contains_key(&shard_id)
    }

    /// This node's ADR-047 [`IntentStore`] for `shard_id`, if the shard has
    /// been created here. The 5c producer records intents through this handle;
    /// the 5b-rpc aux dispatcher serves peers from the same one. `None` for a
    /// shard this node does not host.
    #[must_use]
    pub fn intent_store(&self, shard_id: ShardId) -> Option<Arc<dyn IntentStore>> {
        self.intent_stores
            .lock()
            .lock_or_die("raft_shard_store.intent_stores")
            .get(&shard_id)
            .map(Arc::clone)
    }

    /// Build a [`TransportIntentGatherer`] for `shard_id` (ADR-047 phase
    /// 5b-rpc, client side).
    ///
    /// Derives the shard's voter node ids from the live Raft membership,
    /// **drops the local node**, and resolves each remaining voter to its
    /// transport addr via the `peers` map (a voter with no addr entry is
    /// dropped — it cannot be reached). The gatherer fans the two intent tags
    /// to those peers; an unreachable one is skipped, never fabricated.
    ///
    /// Returns `None` for a shard this node does not host. The committer's
    /// `cluster_size` is `gatherer.peer_count() + 1` (the peers plus self).
    ///
    /// Driven by the per-shard committer task spawned in `create_shard` when
    /// the intent store opens durably.
    #[must_use]
    pub fn intent_gatherer(&self, shard_id: ShardId) -> Option<TransportIntentGatherer> {
        let peers = self.resolve_voter_peers(shard_id)?;
        // TLS is not plumbed into kiseki-log's transport yet (the Raft network
        // uses plaintext `TcpNetworkFactory::new`); pass `None`. The field is
        // here for the mTLS seam when TLS lands.
        Some(TransportIntentGatherer::new(shard_id, peers, None))
    }

    /// Resolve a shard's voter peers (live Raft voters minus the local node,
    /// each mapped to its transport addr). Shared by [`Self::intent_gatherer`]
    /// (the read-side gather) and [`Self::put_intent_and_fan`] (the write-side
    /// fan), so both fan to exactly the same voter set.
    ///
    /// A voter with no addr in the `peers` map is dropped — it cannot be
    /// reached. Returns `None` for a shard this node does not host.
    fn resolve_voter_peers(&self, shard_id: ShardId) -> Option<Vec<(NodeId, String)>> {
        let store = self.get_shard(shard_id).ok()?;
        let local = self.node_id;
        Some(
            store
                .voter_ids()
                .into_iter()
                .filter(|id| *id != local)
                .filter_map(|id| self.peers.get(&id).map(|addr| (NodeId(id), addr.clone())))
                .collect(),
        )
    }

    /// ADR-047 `LeaderSink` — the quorum intent-write (NO-LOSS CRITICAL).
    ///
    /// Durably records `intent` on a quorum so the gateway can fast-ack a write
    /// BEFORE the synchronous Raft round. Writes the LOCAL per-shard
    /// [`IntentStore`] (one durable copy) and fans the intent to the shard's
    /// voter peers via the W12 intent-fan coalescer; returns `Ok` ONLY once total durable
    /// copies (`1 local + remote acks`) reach `self.min_acks`. Otherwise `Err`
    /// — the caller MUST NOT ack (an acked write is guaranteed on `≥ min_acks`
    /// replicas, I-L2/I-CS1).
    ///
    /// # Fan-includes-leader (MF-3 — closes the no-election orphan)
    /// The fan target set MUST include the **current shard leader**, because the
    /// leader is the sole incorporator (`LeaderSink`) draining its own store: an
    /// acked intent that never reaches the leader would be incorporated by no
    /// one until an unrelated election (R4 violation). So:
    /// - if THIS node is the leader, the local put already covers it — fan the
    ///   rest to reach `min_acks`;
    /// - else, fan the leader FIRST (awaiting its ack), then fan the remaining
    ///   voters to top up to `min_acks`. The leader counts toward `min_acks`.
    /// - if the leader is unknown (election in progress), fall back to fanning
    ///   to `min_acks` voters — the new leader's election recovery (`recover`)
    ///   backstops, re-deriving the intent from the durability quorum.
    ///
    /// # Non-durable-store guard (F-P5b-rpc-1)
    /// If the shard's intent store is the [`InMemIntentStore`] degrade (its
    /// durable open failed), this returns [`LogError::Unavailable`] WITHOUT
    /// writing — acking on a non-durable intent loses the write on crash.
    ///
    /// # Errors
    /// [`LogError::ShardNotFound`] for a shard not hosted here;
    /// [`LogError::Unavailable`] on a non-durable intent store or a local-store
    /// write failure; [`LogError::QuorumLost`] when the durable copies fall
    /// short of `min_acks`.
    pub async fn put_intent_and_fan(
        &self,
        shard_id: ShardId,
        intent: WriteIntent,
    ) -> Result<(), LogError> {
        // ADR-047 hot-path timer (pif.total) — covers the whole body,
        // including the non-durable refuse path so a degraded shard's
        // refusal time is observable. Post-W12 this measures the
        // submitter's wall time: submission + coalesce wait + flush wall.
        kiseki_tracing::hot_timer_guard!(_ht_pif_total = "pif.total");

        // Non-durable guard FIRST: refuse before any write so an in-memory
        // degrade can never produce an acked-but-volatile intent.
        let is_durable = self
            .durable_intent_shards
            .lock()
            .lock_or_die("raft_shard_store.durable_intent_shards")
            .contains(&shard_id);
        if !is_durable {
            tracing::warn!(
                shard_id = %shard_id.0,
                "put_intent_and_fan refused: intent store is non-durable (decoupled-ack would lose data on crash)",
            );
            return Err(LogError::Unavailable);
        }

        // W12 (2026-06-02): route through the per-shard intent-fan
        // coalescer. The coalescer owns the local `put_batch` + the fan,
        // amortising both across up to `KISEKI_INTENT_FAN_BATCH_MAX`
        // concurrent submissions. The submitter just sends + awaits a
        // oneshot.
        let coalescer = {
            self.intent_fan_coalescers
                .lock()
                .lock_or_die("raft_shard_store.intent_fan_coalescers")
                .get(&shard_id)
                .cloned()
        }
        .ok_or(LogError::ShardNotFound(shard_id))?;
        coalescer.submit(intent).await
    }

    /// Stop every spawned per-shard committer task cleanly: signal shutdown on
    /// each watch channel, then join the dedicated thread. Idempotent — a
    /// second call finds the map empty. Called by `Drop` and available for an
    /// explicit graceful shutdown.
    pub fn shutdown(&self) {
        let handles: Vec<(ShardId, ShardCommitterHandle)> = {
            let mut guard = self
                .committers
                .lock()
                .lock_or_die("raft_shard_store.committers");
            guard.drain().collect()
        };
        for (shard_id, handle) in handles {
            // Best-effort: a closed channel (receiver already gone) just means
            // the loop already exited.
            let _ = handle.shutdown.send(true);
            if handle.join.join().is_err() {
                tracing::warn!(shard_id = %shard_id.0, "shard committer thread panicked on shutdown");
            }
        }
    }

    /// Stop and join a single shard's committer supervisor, removing it from the
    /// `committers` map (ADR-047 `LeaderSink`, MF-10). Idempotent — a no-op if the
    /// shard never spawned one (gate off / non-durable) or was already stopped.
    ///
    /// This is the teardown half of the leader-only lifecycle: the supervisor
    /// thread must NOT outlive its shard. Called by [`Self::retire_shard`].
    fn stop_committer(&self, shard_id: ShardId) {
        let handle = self
            .committers
            .lock()
            .lock_or_die("raft_shard_store.committers")
            .remove(&shard_id);
        if let Some(handle) = handle {
            let _ = handle.shutdown.send(true);
            if handle.join.join().is_err() {
                tracing::warn!(shard_id = %shard_id.0, "shard committer thread panicked on retire");
            }
        }
    }

    /// Retire a shard hosted on this node (ADR-047 `LeaderSink`, MF-10): stop its
    /// committer supervisor, unregister its Raft handle + `IntentSync` aux
    /// dispatcher from the multiplexed listener, and drop its per-shard state
    /// (intent store + durable flag + shard handle). Idempotent.
    ///
    /// Wired so the leader-only committer can never outlive its shard. Split /
    /// merge production wiring is not exercised single-shard today (MF-10 is
    /// forward-looking), but the teardown is in place so a future retire path
    /// only has to call this.
    pub fn retire_shard(&self, shard_id: ShardId) {
        // Stop the committer FIRST so it cannot incorporate against a
        // half-torn-down shard.
        self.stop_committer(shard_id);

        // Unregister the Raft handle + aux dispatcher from the listener.
        if let Some(reg) = self
            .listener_registry
            .lock()
            .lock_or_die("raft_shard_store.listener_registry")
            .as_ref()
        {
            reg.unregister_aux(shard_id);
            reg.unregister_shard(shard_id);
        }

        // Drop per-shard state.
        self.intent_stores
            .lock()
            .lock_or_die("raft_shard_store.intent_stores")
            .remove(&shard_id);
        self.durable_intent_shards
            .lock()
            .lock_or_die("raft_shard_store.durable_intent_shards")
            .remove(&shard_id);
        // W12: drop the coalescer handle — closes the mpsc channel, the
        // background task observes the closure and exits cleanly.
        self.intent_fan_coalescers
            .lock()
            .lock_or_die("raft_shard_store.intent_fan_coalescers")
            .remove(&shard_id);
        // Lever 1: same teardown for the receiver-side coalescer.
        self.intent_recv_coalescers
            .lock()
            .lock_or_die("raft_shard_store.intent_recv_coalescers")
            .remove(&shard_id);
        self.shards
            .lock()
            .lock_or_die("raft_shard_store.shards")
            .remove(&shard_id);
        tracing::info!(shard_id = %shard_id.0, "shard retired (committer stopped, registry + state dropped)");
    }

    /// Create a shard's Raft group on this node.
    ///
    /// **Does not call `Raft::initialize()`** — the per-shard handle
    /// is registered with the multiplexed listener immediately so
    /// inbound RPCs can dispatch, but membership setup is a separate
    /// step ([`Self::initialize_shard`]). This decoupling is what
    /// makes ADR-033 §4 cluster-wide split safe: every node's apply
    /// hook calls `create_shard` for the new shard without blocking
    /// on votes from peers that haven't yet applied the same
    /// `RecordSplit`. Once every replica has the shard registered,
    /// the leader of the new shard explicitly calls
    /// `initialize_shard`.
    ///
    /// Optionally spawns the Raft RPC server on `raft_addr`.
    ///
    /// # Panics
    ///
    /// Panics if the Raft instance fails to construct (out of memory
    /// or unrecoverable openraft config error — both fatal at boot).
    #[allow(clippy::too_many_lines)] // boot-time per-shard wiring is genuinely many steps
    pub fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        _node_id: NodeId,
        _config: ShardConfig,
        raft_addr: Option<&str>,
    ) {
        let peers = self.peers.clone();
        let node_id = self.node_id;
        let data_dir = self.data_dir.clone();
        let inline_store = self.inline_store.clone();

        // Lazy-init the per-node Raft RPC listener on the first call
        // with `raft_addr=Some(...)`. Subsequent shards register
        // through the same listener via the cloned `RegistryHandle`
        // — ADR-041 §"Lifecycle".
        let registry = if let Some(addr) = raft_addr {
            Some(self.ensure_listener_started(addr))
        } else {
            self.listener_registry
                .lock()
                .lock_or_die("raft_shard_store.listener_registry")
                .as_ref()
                .cloned()
        };

        let handle = self.rt.handle().clone();
        let store = std::thread::spawn(move || {
            handle.block_on(async {
                // Bounded retry on `StoreConstruction` errors. The
                // failure modes we've actually seen in CI / e2e are
                // transient: fjall's open() can race against a
                // sibling process tearing down a stale lock; openraft's
                // multi-node `Raft::new` can race against the
                // multiplexed listener's binding when 3 nodes spawn at
                // once in docker compose. A small retry budget (5
                // attempts × 500 ms = 2.5 s) covers both without
                // masking a genuinely broken on-disk state, which
                // would still fail every attempt and surface the
                // underlying error in the final panic message.
                //
                // Boot-fatal errors (out of memory, openraft config
                // bug) still panic — but with the real cause in the
                // message instead of the opaque "failed to create
                // Raft log store" the call site used to print.
                const MAX_ATTEMPTS: u32 = 5;
                const RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
                let mut last_err: Option<LogError> = None;
                for attempt in 1..=MAX_ATTEMPTS {
                    match OpenRaftLogStore::new(
                        node_id,
                        shard_id,
                        tenant_id,
                        &peers,
                        data_dir.as_deref(),
                        inline_store.clone(),
                    )
                    .await
                    {
                        Ok(s) => return Arc::new(s),
                        Err(e) => {
                            tracing::warn!(
                                shard_id = %shard_id.0,
                                attempt,
                                max = MAX_ATTEMPTS,
                                error = %e,
                                "OpenRaftLogStore::new failed — retrying",
                            );
                            last_err = Some(e);
                            tokio::time::sleep(RETRY_INTERVAL).await;
                        }
                    }
                }
                // Exhausted — fail-stop with the real error in the
                // message so operators don't need to dig through
                // container logs.
                panic!(
                    "OpenRaftLogStore::new failed after {MAX_ATTEMPTS} attempts for shard {}: {}",
                    shard_id.0,
                    last_err.map_or_else(|| "no error recorded".to_string(), |e| e.to_string()),
                );
            })
        })
        .join()
        .expect("Raft shard creation thread panicked (system failure — see prior panic)");

        // ADR-047: create this shard's IntentStore so peers can SERVE their
        // intent state via the IntentSync aux dispatcher AND the local
        // producer can record + fan intents for fast-ack. Durable
        // (FjallIntentStore at `<data_dir>/<shard_id>/intents`) when a data
        // dir is set, else in-memory — mirroring the shard log store's
        // persistence choice above.
        //
        // F-P5b-rpc-1 obligation (no-loss / O3): once the producer fast-acks
        // a write on its intent being quorum-DURABLE, an in-memory intent
        // store would lose acked writes on crash — violating ADR-047's core
        // guarantee. With the capability gate removed, decoupled-ack is THE
        // write path for async-eligible surfaces, so a durable-open failure
        // is a HARD ERROR for the shard (no silent degrade-to-volatile).
        //
        // `data_dir = None` is the explicit "in-memory test cluster" knob —
        // the shard is intentionally non-durable; the supervisor below
        // simply does not spawn and `put_intent_and_fan` refuses with
        // `LogError::Unavailable`.
        let (intent_store, intent_store_durable): (Arc<dyn IntentStore>, bool) =
            match &self.data_dir {
                Some(dir) => {
                    let path = dir.join(shard_id.0.to_string()).join("intents");
                    let store = FjallIntentStore::open(&path).unwrap_or_else(|e| {
                        panic!(
                            "durable IntentStore open failed for shard {} at {}: {} \
                             (ADR-047 F-P5b-rpc-1: cannot host a shard whose intent \
                              store is non-durable — decoupled-ack is the write path)",
                            shard_id.0,
                            path.display(),
                            e,
                        )
                    });
                    (Arc::new(store), true)
                }
                // `data_dir = None` (in-memory test cluster) is volatile, so it is
                // NOT durable for decoupled-ack purposes.
                None => (Arc::new(InMemIntentStore::new()), false),
            };
        self.intent_stores
            .lock()
            .lock_or_die("raft_shard_store.intent_stores")
            .insert(shard_id, Arc::clone(&intent_store));
        if intent_store_durable {
            self.durable_intent_shards
                .lock()
                .lock_or_die("raft_shard_store.durable_intent_shards")
                .insert(shard_id);
        }

        // Lever 1 (2026-06-02): spawn the per-shard receiver-side
        // intent_put coalescer on the Raft runtime. The dispatcher we
        // hand to the listener routes inbound `intent_put` RPCs
        // through it so multiple producers' RPCs share one fjall
        // put_batch per coalesce window. Only spawned for durable
        // intent stores — a non-durable shard refuses
        // put_intent_and_fan upstream anyway and would never receive a
        // production intent_put fan.
        let recv_coalescer = if intent_store_durable {
            let coalescer = crate::intent_recv_coalescer::spawn(
                self.rt.handle(),
                crate::intent_recv_coalescer::RecvConfig {
                    shard_id,
                    store: Arc::clone(&intent_store),
                    cap_max: crate::intent_recv_coalescer::recv_batch_max_from_env(),
                    cap_timeout: crate::intent_recv_coalescer::recv_batch_timeout_from_env(),
                },
            );
            self.intent_recv_coalescers
                .lock()
                .lock_or_die("raft_shard_store.intent_recv_coalescers")
                .insert(shard_id, coalescer.clone());
            Some(Arc::new(coalescer))
        } else {
            None
        };

        // Register this shard's Raft handle with the listener so
        // inbound multiplexed RPCs route here.
        if let Some(reg) = registry {
            reg.register_shard(shard_id, store.raft_handle());
            // ADR-047 aux dispatcher: register the IntentSync handler on the
            // SAME shard so a peer's committer can read this node's intent
            // state over the multiplexed transport (and now also receive a
            // fanned `intent_put`). The listener consults it only after the
            // Raft dispatcher returns `UnknownTag`, so the consensus path is
            // untouched.
            reg.register_aux(
                shard_id,
                build_intent_dispatcher(Arc::clone(&intent_store), recv_coalescer.clone()),
            );
            tracing::info!(shard_id = %shard_id.0, "shard registered with Raft RPC listener");
        }

        {
            let mut shards = self.shards.lock().lock_or_die("raft_shard_store.shards");
            shards.insert(shard_id, Arc::clone(&store));
        }

        // ADR-047 `LeaderSink`: spawn this shard's committer SUPERVISOR when
        // the intent store is durable (F-P5b-rpc-1 — incorporating from a
        // non-durable store could surface an intent a crash would lose). For
        // an in-memory test shard (`data_dir = None`) the supervisor does
        // not spawn — `put_intent_and_fan` refuses with `Unavailable` and
        // tests stay on the synchronous append path.
        //
        // The supervisor runs on EVERY node hosting the shard: it drains the
        // log only while this node is the Raft leader (running recover() once
        // on becoming leader), and self-prunes the local intent store on every
        // node (leader or follower) against the applied max_incorporated_seq.
        if intent_store_durable {
            self.spawn_supervisor(shard_id, &store, Arc::clone(&intent_store));
            // W12 (2026-06-02): producer-side intent-fan coalescer. Same
            // F-P5b-rpc-1 gate as the supervisor — only durable intent
            // stores get batching (a non-durable store refuses
            // `put_intent_and_fan` outright in the public API).
            self.spawn_intent_fan_coalescer(shard_id, &store, intent_store);
        }
    }

    /// W12 (2026-06-02): spawn the per-shard intent-fan coalescer task on
    /// the Raft runtime. The submission handle lives in
    /// `self.intent_fan_coalescers` for `put_intent_and_fan` to look up
    /// per call.
    ///
    /// The resolver closure re-reads voter membership + current leader on
    /// every batch flush so a recent membership change or leadership move
    /// is honoured without restart.
    fn spawn_intent_fan_coalescer(
        &self,
        shard_id: ShardId,
        store: &Arc<OpenRaftLogStore>,
        intent_store: Arc<dyn IntentStore>,
    ) {
        let resolver_store = Arc::clone(store);
        let resolver_peers = self.peers.clone();
        let local = self.node_id;
        let resolver: crate::intent_fan_coalescer::PeerLeaderResolver = Arc::new(move || {
            let peers: Vec<(NodeId, String)> = resolver_store
                .voter_ids()
                .into_iter()
                .filter(|id| *id != local)
                .filter_map(|id| {
                    resolver_peers
                        .get(&id)
                        .map(|addr| (NodeId(id), addr.clone()))
                })
                .collect();
            let leader_id = resolver_store.current_leader_id();
            (peers, leader_id)
        });
        let coalescer = crate::intent_fan_coalescer::spawn(
            self.rt.handle(),
            crate::intent_fan_coalescer::CoalescerConfig {
                shard_id,
                local_node: NodeId(self.node_id),
                store: intent_store,
                resolver,
                min_acks: self.min_acks,
                cap_max: crate::intent_fan_coalescer::batch_max_from_env(),
                cap_timeout: crate::intent_fan_coalescer::batch_timeout_from_env(),
                peer_rpc_timeout: INTENT_FAN_PEER_TIMEOUT,
            },
        );
        self.intent_fan_coalescers
            .lock()
            .lock_or_die("raft_shard_store.intent_fan_coalescers")
            .insert(shard_id, coalescer);
    }

    /// Spawn the per-shard committer **supervisor** (ADR-047 `LeaderSink`, MF-2 +
    /// MF-5) on a DEDICATED `std::thread` holding the Raft runtime handle. The
    /// supervisor drives the synchronous [`RaftLogIncorporationSink`], whose
    /// [`Handle::block_on`](tokio::runtime::Handle::block_on) MUST NOT run on a
    /// tokio worker (the `raft_intent_sink` threading contract) — hence the
    /// dedicated thread. Its shutdown sender + join handle are kept in
    /// `committers` so [`Self::shutdown`] / `Drop` (and shard retire) can stop
    /// it cleanly.
    ///
    /// **The supervisor runs on every node hosting the shard** and tracks Raft
    /// leadership each tick (polling `OpenRaftLogStore::is_leader` off the live
    /// metrics watch):
    ///
    /// - **On becoming leader** (`was_leader == false → is_leader == true`): run
    ///   `recover()` ONCE — gather pending intents from peers via
    ///   [`PeerIntentGatherer::gather_pending`], union with local, restore into
    ///   the local store — BEFORE resuming steady-state draining, so the new
    ///   leader holds every acked intent (R1/R5 no-loss). If recovery is
    ///   sub-threshold ([`IntentError::InsufficientQuorum`]) it is retried each
    ///   tick until it succeeds; draining does NOT start until recovery lands.
    /// - **While leader**: `drain_local()` each tick (`LeaderSink` steady state).
    /// - **On losing leadership**: stop draining (idle). A deposed leader's
    ///   `client_write` is fenced by openraft anyway; we simply stop trying.
    /// - **On every node, every tick (MF-5 follower self-prune)**: prune the
    ///   local intent store up to this node's OWN applied `max_incorporated_seq`
    ///   (read from the replicated log). Once a seq is incorporated and
    ///   replicated to this node, its intent copy is redundant — pruning is safe
    ///   everywhere and bounds follower-store growth under sustained load.
    ///
    /// `cluster_size` is the configured voter count (`self.peers.len()`, incl.
    /// self): the authoritative membership size, stable even before the shard's
    /// Raft membership is initialized. `min_acks` sets the recovery threshold.
    fn spawn_supervisor(
        &self,
        shard_id: ShardId,
        store: &Arc<OpenRaftLogStore>,
        intent_store: Arc<dyn IntentStore>,
    ) {
        let cluster_size = self.peers.len().max(1);
        let min_acks = self.min_acks;
        // The supervisor spawns at create_shard time — BEFORE membership is
        // initialized — so a static peer snapshot would be empty forever. Build
        // a LIVE-resolving gatherer: each gather re-reads the shard's voter set
        // (minus self) from the live Raft membership, mapped through `peers`.
        let resolver_store = Arc::clone(store);
        let resolver_peers = self.peers.clone();
        let local = self.node_id;
        let resolver: Arc<dyn Fn() -> Vec<(NodeId, String)> + Send + Sync> = Arc::new(move || {
            resolver_store
                .voter_ids()
                .into_iter()
                .filter(|id| *id != local)
                .filter_map(|id| {
                    resolver_peers
                        .get(&id)
                        .map(|addr| (NodeId(id), addr.clone()))
                })
                .collect()
        });
        let gatherer = TransportIntentGatherer::with_resolver(shard_id, resolver, None);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let raft_handle = self.rt.handle().clone();
        let appender = Arc::clone(store);
        let leadership_store = Arc::clone(store);
        // A separate handle to the SAME intent store, for the per-node
        // self-prune (the committer owns its own clone for incorporation).
        let prune_store = Arc::clone(&intent_store);
        let join = std::thread::Builder::new()
            .name(format!("kiseki-committer-{}", shard_id.0))
            .spawn(move || {
                raft_handle.clone().block_on(async move {
                    // The sink bridges the sync committer to the async Raft log;
                    // `new` seeds the F-2 recovery floor via block_on, safe here
                    // because this is a dedicated std::thread, not a tokio
                    // worker (the threading contract).
                    let sink = RaftLogIncorporationSink::new(
                        OpenRaftAppender { store: appender },
                        raft_handle.clone(),
                    );
                    let committer = ShardCommitter::new(intent_store, sink, cluster_size, min_acks);
                    run_supervisor_loop(
                        shard_id,
                        committer,
                        gatherer,
                        leadership_store,
                        prune_store,
                        COMMITTER_INTERVAL,
                        shutdown_rx,
                    )
                    .await;
                });
            })
            .expect("failed to spawn `LeaderSink` committer supervisor thread");
        self.committers
            .lock()
            .lock_or_die("raft_shard_store.committers")
            .insert(
                shard_id,
                ShardCommitterHandle {
                    shutdown: shutdown_tx,
                    join,
                },
            );
        tracing::info!(shard_id = %shard_id.0, cluster_size, min_acks, "LeaderSink committer supervisor spawned (ADR-047)");
    }

    /// Initialize the Raft membership for a shard's group. Must be
    /// called only on the seed node, only after every replica has
    /// registered the shard via `create_shard` (otherwise the seed's
    /// vote requests race against peer registration and time out).
    ///
    /// Idempotent: openraft returns `NotAllowed` once a group is
    /// initialized, which we map to `Ok(())` in
    /// `OpenRaftLogStore::initialize_membership`.
    pub fn initialize_shard(&self, shard_id: ShardId) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        let peers = self.peers.clone();
        let handle = self.rt.handle().clone();
        let res = std::thread::spawn(move || {
            handle.block_on(async move { store.initialize_membership(&peers).await })
        })
        .join()
        .map_err(|_| LogError::Unavailable)?;
        if res.is_ok() {
            tracing::info!(
                shard_id = %shard_id.0,
                "shard membership initialized",
            );
        }
        res
    }

    /// Async variant of [`Self::initialize_shard`] for callers that
    /// already run on the Raft runtime. Avoids the
    /// `thread::spawn + join` blocking dance.
    pub async fn initialize_shard_async(&self, shard_id: ShardId) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.initialize_membership(&self.peers).await
    }

    /// Snapshot the current set of shard ids the store knows about.
    /// Used by the runtime's periodic ADR-030 inline-threshold
    /// recompute loop so it can iterate without holding any locks
    /// across the recompute work.
    #[must_use]
    pub fn shard_ids(&self) -> Vec<ShardId> {
        self.shards
            .lock()
            .lock_or_die("raft_shard_store.shards")
            .keys()
            .copied()
            .collect()
    }

    /// `true` when this node is the elected leader of `shard_id`.
    /// Returns `false` for unknown shards instead of erroring so the
    /// recompute loop can no-op cleanly on a shard that has not yet
    /// completed bootstrap.
    #[must_use]
    pub fn is_shard_leader(&self, shard_id: ShardId) -> bool {
        let Ok(store) = self.get_shard(shard_id) else {
            return false;
        };
        store.is_leader()
    }

    /// ADR-030 leader-side recompute helper — Raft-commit a new
    /// `ShardConfig` for `shard_id`, awaiting consensus on the Raft
    /// runtime. Returns `Err` if this node isn't the leader (caller
    /// should have gated by `is_shard_leader` first) or if the commit
    /// fails. The committed config replicates to followers via the
    /// existing apply hook in the state machine.
    pub fn submit_shard_config(
        &self,
        shard_id: ShardId,
        config: ShardConfig,
    ) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        self.run_blocking(&store, move |s| {
            Box::pin(async move { s.set_shard_config(config).await })
        })?
    }

    /// Look up a shard's Raft store.
    fn get_shard(&self, shard_id: ShardId) -> Result<Arc<OpenRaftLogStore>, LogError> {
        let shards = self.shards.lock().lock_or_die("raft_shard_store.shards");
        shards
            .get(&shard_id)
            .cloned()
            .ok_or(LogError::ShardNotFound(shard_id))
    }

    /// Run an async store call on the Raft runtime from a sync trait
    /// method. Spawns a transient OS thread + `block_on` so the call
    /// neither nests inside the calling tokio runtime nor borrows
    /// `&self` past the closure body.
    fn run_blocking<F, T>(&self, store: &Arc<OpenRaftLogStore>, f: F) -> Result<T, LogError>
    where
        F: FnOnce(Arc<OpenRaftLogStore>) -> std::pin::Pin<Box<dyn Future<Output = T> + Send>>
            + Send
            + 'static,
        T: Send + 'static,
    {
        let s = Arc::clone(store);
        let handle = self.rt.handle().clone();
        std::thread::spawn(move || handle.block_on(f(s)))
            .join()
            .map_err(|_| LogError::Unavailable)
    }

    /// Sync helper for `OpenRaftLogStore::shard_health` from sync
    /// trait methods.
    fn shard_health_blocking(&self, store: &Arc<OpenRaftLogStore>) -> Result<ShardInfo, LogError> {
        self.run_blocking(store, |s| Box::pin(async move { s.shard_health().await }))
    }

    /// Sync helper for `OpenRaftLogStore::set_shard_range` from sync
    /// trait methods. Errors from the Raft write are logged but
    /// swallowed — the trait method has no error channel; production
    /// callers needing strict propagation should use
    /// `LogOps::split_shard` / `merge_shards` which return `Result`.
    fn set_shard_range_blocking(
        &self,
        store: &Arc<OpenRaftLogStore>,
        range_start: [u8; 32],
        range_end: [u8; 32],
    ) -> Result<(), LogError> {
        self.run_blocking(store, move |s| {
            Box::pin(async move {
                if let Err(e) = s.set_shard_range(range_start, range_end).await {
                    tracing::warn!(error = %e, "set_shard_range_blocking: Raft write failed");
                }
            })
        })
    }

    /// Sync helper for `OpenRaftLogStore::set_shard_state`.
    fn set_shard_state_blocking(
        &self,
        store: &Arc<OpenRaftLogStore>,
        state: ShardState,
    ) -> Result<(), LogError> {
        self.run_blocking(store, move |s| {
            Box::pin(async move {
                if let Err(e) = s.set_shard_state(state).await {
                    tracing::warn!(error = %e, "set_shard_state_blocking: Raft write failed");
                }
            })
        })
    }
}

/// PART 8 §W — bounded post-promotion wait. After a false→true leadership
/// edge, the supervisor polls `applied_log_index >= committed_log_index` for
/// up to this duration before resuming drain. On timeout it logs a warn and
/// proceeds (do NOT deadlock — the SM gate still catches duplicates).
const POST_PROMOTION_WAIT_MAX: Duration = Duration::from_secs(5);

/// PART 8 §W — poll cadence for the post-promotion wait.
const POST_PROMOTION_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// The per-shard committer supervisor loop (ADR-047 `LeaderSink`, PART 8).
///
/// Runs on a dedicated thread (the threading contract — drives a synchronous
/// [`RaftLogIncorporationSink`] that `block_on`s the async log). Each tick:
///
/// 1. **Per-intent self-prune (every node, PART 8 §T):** snapshot the SM's
///    `recent_incorporated_seqs`; for each seq in the snapshot, call
///    [`IntentStore::remove_seq`] on the local store. Bounded by the
///    snapshot size. The SM owns the dedup set (replicated, snapshot-included);
///    the supervisor reads it off-band and prunes only intents *known to be
///    applied on this replica*. Preserves the Raft/store fault isolation that
///    a per-apply prune would lose (Finding T).
/// 2. **Leadership transition + post-promotion wait-for-current (§W):** read
///    `is_leader()`. On the false→true edge, BEFORE running `recover()`, poll
///    `applied_log_index >= committed_log_index` for up to
///    [`POST_PROMOTION_WAIT_MAX`] so the recent-set covers everything in the
///    log before draining gates new appends. On the true→false edge, stop
///    draining (idle until re-elected).
/// 3. **Election recovery + recovery dedup:** gather + `recover()` (filtered to
///    drop already-incorporated and ancient intents on the way in — see §6).
/// 4. **Drain (leader only, after recovery succeeded):** `drain_local()`.
///
/// Per-tick errors are logged and swallowed so one bad pass never kills the
/// loop. Shutdown is via the `shutdown` watch (set by `Self::shutdown` / `Drop`
/// / shard retire).
#[allow(clippy::too_many_lines)] // supervisor cohesion > arbitrary line cap
async fn run_supervisor_loop<S, G>(
    shard_id: ShardId,
    mut committer: ShardCommitter<S>,
    gatherer: G,
    leadership_store: Arc<OpenRaftLogStore>,
    prune_store: Arc<dyn IntentStore>,
    interval: Duration,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) where
    S: crate::intent_committer::IncorporationSink,
    G: PeerIntentGatherer,
{
    if *shutdown.borrow() {
        return;
    }
    let mut was_leader = false;
    let mut recovered_this_term = false;

    loop {
        // --- (1) Per-intent self-prune on every node (PART 8 §T) ----------
        // Snapshot the SM's recent_incorporated set and remove exactly those
        // local intents. Only intents already replicated AND applied on this
        // node disappear; everything else stays for recovery to re-derive.
        //
        // W7 (2026-06-02): batched. The 2026-06-02 GCP profile showed the
        // per-element `remove_seq` loop saturating every storage-node core
        // (71.75% CPU in `FjallIntentStore::remove_seq`). Each call paid one
        // fjall `get` + one batch commit; most snapshot entries were no-ops
        // because the local prune had already caught up. `remove_seqs`
        // collapses the whole snapshot to one mutex + one batch commit.
        let snapshot = leadership_store.recent_incorporated_snapshot().await;
        if !snapshot.is_empty() {
            let seqs: Vec<crate::intent::PerspectiveSeq> = snapshot
                .iter()
                .map(|s| crate::intent::PerspectiveSeq(*s))
                .collect();
            if let Err(e) = prune_store.remove_seqs(&seqs) {
                tracing::debug!(shard_id = %shard_id.0, error = %e, "per-intent self-prune failed; retry next tick");
            }
        }

        // --- (2) Leadership transition ------------------------------------
        let is_leader = leadership_store.is_leader();
        if is_leader && !was_leader {
            recovered_this_term = false;
            tracing::info!(shard_id = %shard_id.0, "LeaderSink: became shard leader — waiting for applied catch-up");
            // §W — bounded wait for the SM to apply the inherited log so the
            // recent_incorporated set covers it before we gate new appends.
            let start = std::time::Instant::now();
            loop {
                let applied_idx = leadership_store.applied_log_index();
                let highest_committed = leadership_store.committed_log_index();
                if applied_idx >= highest_committed {
                    break;
                }
                if start.elapsed() >= POST_PROMOTION_WAIT_MAX {
                    tracing::warn!(
                        shard_id = %shard_id.0,
                        applied = applied_idx, committed = highest_committed,
                        "LeaderSink: post-promotion wait timeout — proceeding (SM gate still catches duplicates)",
                    );
                    break;
                }
                tokio::time::sleep(POST_PROMOTION_POLL_INTERVAL).await;
            }
        } else if !is_leader && was_leader {
            tracing::info!(shard_id = %shard_id.0, "LeaderSink: lost shard leadership — parking committer");
        }
        was_leader = is_leader;

        if is_leader {
            // (2a) Election recovery — once per term, retried until it lands.
            if !recovered_this_term {
                match gatherer.gather_pending().await {
                    Ok(peers) => {
                        // PART 8 §6 — recovery dedup. Drop any re-gathered
                        // intent whose seq is in the SM's recent set (already
                        // incorporated on this node) or whose source's
                        // sentinel falls below the ancient cutoff. Re-read
                        // the snapshot AFTER any wait so it reflects the
                        // applied log.
                        let recent_snap = leadership_store.recent_incorporated_snapshot().await;
                        let cutoff = leadership_store.ancient_cutoff_log_index().await;
                        let filtered: Vec<(NodeId, Vec<WriteIntent>)> = peers
                            .into_iter()
                            .map(|(node, set)| {
                                let kept: Vec<WriteIntent> = set
                                    .into_iter()
                                    .filter(|intent| {
                                        let seq = intent.perspective_seq.0;
                                        // Already applied here -> drop.
                                        if recent_snap.contains(&seq) {
                                            return false;
                                        }
                                        // The recovery filter for "ancient"
                                        // is operationally suspicious. Without
                                        // a log-index attached to the recovered
                                        // intent we can't compare against
                                        // cutoff directly, but a non-zero
                                        // cutoff with an empty recent set is
                                        // already odd; we surface that as an
                                        // alarm + drop only when the cutoff
                                        // is non-zero AND this seq pre-dates
                                        // every recent-set entry's physical_ms
                                        // — a heuristic for Finding Q's
                                        // partition-recovery case.
                                        if cutoff > 0 {
                                            if let Some(min_recent) = recent_snap
                                                .iter()
                                                .min_by_key(|h| h.physical_ms)
                                            {
                                                if seq.physical_ms < min_recent.physical_ms.saturating_sub(1) {
                                                    tracing::warn!(
                                                        shard_id = %shard_id.0,
                                                        seq = ?seq,
                                                        cutoff,
                                                        "LeaderSink: recovery dropped ancient intent (Finding Q alarm)",
                                                    );
                                                    return false;
                                                }
                                            }
                                        }
                                        true
                                    })
                                    .collect();
                                (node, kept)
                            })
                            .collect();
                        match committer.recover(&filtered) {
                            Ok(restored) => {
                                recovered_this_term = true;
                                tracing::info!(
                                    shard_id = %shard_id.0,
                                    restored,
                                    "LeaderSink: election recovery complete — resuming drain",
                                );
                            }
                            Err(crate::intent::IntentError::InsufficientQuorum { have, need }) => {
                                tracing::warn!(
                                    shard_id = %shard_id.0, have, need,
                                    "LeaderSink: recovery gather below threshold — retrying, NOT draining",
                                );
                            }
                            Err(e) => {
                                tracing::warn!(shard_id = %shard_id.0, error = %e, "LeaderSink: recovery restore failed — retrying");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(shard_id = %shard_id.0, error = %e, "LeaderSink: recovery peer gather failed — retrying");
                    }
                }
            }

            // (2b/3) Steady-state drain — only after recovery for this term.
            if recovered_this_term {
                if let Err(e) = committer.drain_local() {
                    tracing::warn!(shard_id = %shard_id.0, error = %e, "LeaderSink: drain failed; retrying next tick");
                }
            }
        }

        // Sleep `interval`, waking immediately on shutdown.
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            res = shutdown.changed() => {
                if res.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

impl Drop for RaftShardStore {
    /// Stop every spawned per-shard committer on drop so no dedicated thread
    /// outlives the store. `shutdown` is idempotent with an explicit call.
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// #123 — log `ForwardToLeader` / `LeaderUnavailable` returns from the
/// per-shard append paths at DEBUG, not WARN.
///
/// Both variants are routine control flow on the #111 forwarding write
/// path: when a write reaches a follower, the openraft store maps the
/// `ClientWriteError::ForwardToLeader` hint onto either
/// [`LogError::ForwardToLeader`] (the `*_with_forwarding` methods —
/// surfaces the leader's node id so the caller can re-issue) or
/// [`LogError::LeaderUnavailable`] (the legacy methods — collapses the
/// hint per back-compat). The gateway's `log_bridge` re-issues against
/// the leader via the `AppendForwarder`; the write then commits cleanly.
/// Emitting WARN for this case produced ~22 k log lines per benchmark on
/// the 2026-05-27 GCP run, polluting signal on the hot path.
///
/// Genuine errors (shard busy, key out of range, quorum lost, I/O, etc.)
/// still emit WARN.
fn log_append_err(e: &LogError, ctx: &str) {
    match e {
        LogError::ForwardToLeader { .. } | LogError::LeaderUnavailable(_) => {
            tracing::debug!(error = %e, ctx, "log append: routine forward to leader");
        }
        _ => {
            tracing::warn!(error = %e, ctx, "log append failed");
        }
    }
}

#[async_trait::async_trait]
impl LogOps for RaftShardStore {
    /// ADR-047 decoupled-ack — the real quorum intent-write. Delegates to the
    /// inherent [`RaftShardStore::put_intent_and_fan`].
    async fn put_intent_and_fan(
        &self,
        shard_id: ShardId,
        intent: WriteIntent,
    ) -> Result<(), LogError> {
        RaftShardStore::put_intent_and_fan(self, shard_id, intent).await
    }

    #[tracing::instrument(skip(self, req), fields(shard_id = %req.shard_id.0, tenant_id = %req.tenant_id.0, op = ?req.operation))]
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.shard_id).inspect_err(|e| {
            tracing::warn!(error = %e, "log append_delta: shard lookup failed");
        })?;
        store.append_delta(req).await.inspect_err(|e| {
            log_append_err(e, "log append_delta");
        })
    }

    /// ADR-042 §4 override: surfaces `LogError::ForwardToLeader` with
    /// the leader's node id instead of collapsing onto
    /// `LeaderUnavailable`. Used by the native server proxy
    /// fallback path (`KISEKI_NATIVE_PROXY_FALLBACK=on`) and by
    /// future S3 307 / NFS referral paths.
    #[tracing::instrument(skip(self, req), fields(shard_id = %req.shard_id.0, tenant_id = %req.tenant_id.0, op = ?req.operation))]
    async fn append_delta_with_forwarding(
        &self,
        req: AppendDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.shard_id).inspect_err(|e| {
            tracing::warn!(error = %e, "log append_delta_with_forwarding: shard lookup failed");
        })?;
        store
            .append_delta_with_forwarding(req)
            .await
            .inspect_err(|e| {
                log_append_err(e, "log append_delta_with_forwarding");
            })
    }

    #[tracing::instrument(skip(self, req), fields(shard_id = %req.delta.shard_id.0, tenant_id = %req.delta.tenant_id.0, op = ?req.delta.operation, new_chunks = req.new_chunks.len()))]
    async fn append_chunk_and_delta(
        &self,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.delta.shard_id).inspect_err(|e| {
            tracing::warn!(error = %e, "log append_chunk_and_delta: shard lookup failed");
        })?;
        store
            .append_chunk_and_delta(req.delta, req.new_chunks)
            .await
            .inspect_err(|e| {
                log_append_err(e, "log append_chunk_and_delta");
            })
    }

    /// ADR-042 §4 override: same as `append_chunk_and_delta` but
    /// surfaces `LogError::ForwardToLeader` with the leader's node id.
    #[tracing::instrument(skip(self, req), fields(shard_id = %req.delta.shard_id.0, tenant_id = %req.delta.tenant_id.0, op = ?req.delta.operation, new_chunks = req.new_chunks.len()))]
    async fn append_chunk_and_delta_with_forwarding(
        &self,
        req: AppendChunkAndDeltaRequest,
    ) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.delta.shard_id).inspect_err(|e| {
            tracing::warn!(
                error = %e,
                "log append_chunk_and_delta_with_forwarding: shard lookup failed",
            );
        })?;
        store
            .append_chunk_and_delta_with_forwarding(req.delta, req.new_chunks)
            .await
            .inspect_err(|e| {
                log_append_err(e, "log append_chunk_and_delta_with_forwarding");
            })
    }

    async fn increment_chunk_refcount(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.increment_chunk_refcount(tenant_id, chunk_id).await
    }

    async fn decrement_chunk_refcount(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<bool, LogError> {
        let store = self.get_shard(shard_id)?;
        store.decrement_chunk_refcount(tenant_id, chunk_id).await
    }

    async fn cluster_chunk_state_get(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<Option<ClusterChunkStateEntry>, LogError> {
        let store = self.get_shard(shard_id)?;
        Ok(store.cluster_chunk_state_get(tenant_id, chunk_id).await)
    }

    async fn cluster_chunk_state_iter(
        &self,
        shard_id: ShardId,
    ) -> Result<Vec<(OrgId, ChunkId, ClusterChunkStateEntry)>, LogError> {
        let store = self.get_shard(shard_id)?;
        Ok(store.cluster_chunk_state_iter().await)
    }

    #[tracing::instrument(skip(self, req), fields(shard_id = %req.shard_id.0, from = req.from.0, to = req.to.0))]
    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let store = self.get_shard(req.shard_id).inspect_err(|e| {
            tracing::warn!(error = %e, "log read_deltas: shard lookup failed");
        })?;
        store.read_deltas(req).await.inspect_err(|e| {
            tracing::warn!(error = %e, "log read_deltas: shard read failed");
        })
    }

    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let store = self.get_shard(shard_id)?;
        let info = store.shard_health().await;
        Ok(info)
    }

    async fn earliest_visible_seq(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(shard_id)?;
        Ok(store.earliest_visible_seq().await)
    }

    async fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.set_maintenance(enabled).await
    }

    async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(shard_id)?;
        store.truncate_log().await
    }

    async fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        let store = self.get_shard(shard_id)?;
        store.compact_shard().await
    }

    fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
        // Delegate to the inherent `create_shard` — registers the
        // per-shard Raft group with the multiplexed listener but
        // does NOT initialize membership. The split-shard flow in
        // `LogOps::split_shard` was the only LogOps caller that
        // needed initialize, and it now goes through the
        // control-plane Raft group's apply hook + an explicit
        // `initialize_shard` after registration converges.
        Self::create_shard(self, shard_id, tenant_id, node_id, config, None);
        // Best-effort: initialize on this node so single-node /
        // legacy paths (in-memory store, persistent store) keep
        // working as a self-contained bootstrap. Multi-node
        // production paths invoke `initialize_shard` explicitly
        // from the `kiseki-server` runtime once every replica is
        // registered.
        if self.peers.len() <= 1 {
            let _ = self.initialize_shard(shard_id);
        }
    }

    fn update_shard_range(&self, shard_id: ShardId, range_start: [u8; 32], range_end: [u8; 32]) {
        // Raft-replicated mutation: every replica converges on the
        // new range so routing stays consistent across follower
        // reads. Errors are logged — the trait is sync and can't
        // surface them. Production splits/merges use the
        // `LogOps::split_shard` / `merge_shards` methods which wrap
        // this with full error handling.
        let Ok(store) = self.get_shard(shard_id) else {
            tracing::warn!(shard_id = %shard_id.0, "update_shard_range: shard not found");
            return;
        };
        let _ = self.set_shard_range_blocking(&store, range_start, range_end);
    }

    fn set_shard_state(&self, shard_id: ShardId, state: ShardState) {
        let Ok(store) = self.get_shard(shard_id) else {
            tracing::warn!(shard_id = %shard_id.0, "set_shard_state: shard not found");
            return;
        };
        let _ = self.set_shard_state_blocking(&store, state);
    }

    fn set_shard_config(&self, shard_id: ShardId, config: ShardConfig) {
        let Ok(store) = self.get_shard(shard_id) else {
            tracing::warn!(shard_id = %shard_id.0, "set_shard_config: shard not found");
            return;
        };
        let _ = self.run_blocking(&store, move |s| {
            Box::pin(async move {
                if let Err(e) = s.set_shard_config(config).await {
                    tracing::warn!(error = %e, "set_shard_config: Raft write failed");
                }
            })
        });
    }

    fn split_shard(
        &self,
        shard_id: ShardId,
        new_shard_id: ShardId,
        node_id: NodeId,
    ) -> Result<ShardId, LogError> {
        // Verify source exists.
        let source = self.get_shard(shard_id)?;
        let info = self.shard_health_blocking(&source)?;

        let mut midpoint = [0u8; 32];
        for (i, mid) in midpoint.iter_mut().enumerate() {
            // Big-endian 256-bit average — same formula as
            // MemShardStore::split_shard at store.rs:240.
            *mid = info.range_start[i] / 2 + info.range_end[i] / 2;
        }

        // Mark the source as `Splitting` BEFORE creating the new
        // shard. The state acts as a cutover gate so concurrent
        // writes can't lose deltas during the redistribution
        // window — `Splitting` is `accepts_writes()` (per
        // ShardState) so writes still land, just with the operator
        // contract that they're being redistributed.
        self.set_shard_state_blocking(&source, ShardState::Splitting)?;

        // Create the new shard's Raft group (upper half). With
        // ADR-033 §4 wired, the per-shard groups for splits are
        // created cluster-wide via the control-plane apply hook
        // BEFORE `LogOps::split_shard` runs — but for single-node
        // setups (and as a fallback when `has_shard` is false) we
        // create + initialize locally here so this method stays
        // self-contained.
        if !self.has_shard(new_shard_id) {
            Self::create_shard(
                self,
                new_shard_id,
                info.tenant_id,
                node_id,
                info.config.clone(),
                None,
            );
            // Single-node / fallback: initialize membership now so
            // the per-shard Raft has a leader that can accept
            // writes during redistribution. Multi-node clusters
            // initialize the new shard via the admin RPC handler
            // *before* calling `LogOps::split_shard`.
            self.initialize_shard(new_shard_id)?;
        }

        // Set the new shard's range = [midpoint, upper_end).
        let new_store = self.get_shard(new_shard_id)?;
        self.set_shard_range_blocking(&new_store, midpoint, info.range_end)?;

        // ADR-033 §3 step 3 — redistribute upper-half deltas from
        // the source to the new shard. Eager replay (option b in the
        // ADR-041 escalation discussion): read source's full delta
        // stream, filter for `hashed_key >= midpoint`, append each to
        // the new shard via Raft consensus. After this returns, the
        // new shard's log holds every upper-range delta the source
        // had at split time. Sequence numbers DO NOT match across
        // shards — each shard's tip is independent — but the
        // composition-level identity of every delta (chunk_refs,
        // payload, hashed_key) is preserved.
        //
        // Concurrent writes during redistribution: writes for keys
        // in [midpoint, upper_end) routed to source by stale shard
        // map caches will be rejected with `KeyOutOfRange` once
        // source's range was tightened below — but we tighten the
        // source's range AFTER redistribution to avoid the in-flight
        // race where source rejects a write whose redistribution
        // pass already completed. Result: writes for upper-half
        // keys during redistribution land on source's tail (lower
        // range still includes upper-half until cutover); the
        // sweep at the end picks them up via a final delta-tip
        // recheck.
        let upper_end = info.range_end;
        let upper_start = midpoint;
        let source_for_replay = Arc::clone(&source);
        let new_for_replay = Arc::clone(&new_store);
        self.run_blocking::<_, Result<u64, LogError>>(&source_for_replay, move |source_ref| {
            let new_store = Arc::clone(&new_for_replay);
            Box::pin(async move {
                redistribute_upper_half(&source_ref, &new_store, upper_start, upper_end).await
            })
        })??;

        // Shrink the source's range to [old_start, midpoint) NOW
        // that all upper-half deltas have been replayed. Subsequent
        // writes for upper-half keys arriving at source will be
        // rejected with `KeyOutOfRange` so the gateway can refresh
        // its shard-map cache and retry to the new shard.
        self.set_shard_range_blocking(&source, info.range_start, midpoint)?;

        // Cutover complete: source is back to `Healthy`. The
        // upper-half deltas physically remain in source's log
        // (immutable per I-L3); they're outside source's current
        // range so they're inert as far as routing is concerned.
        // GC at compaction time can prune them based on the range
        // membership check.
        self.set_shard_state_blocking(&source, ShardState::Healthy)?;
        Ok(new_shard_id)
    }

    fn merge_shards(
        &self,
        target_shard_id: ShardId,
        source_shard_id: ShardId,
    ) -> Result<(), LogError> {
        // Verify both shards exist.
        let target = self.get_shard(target_shard_id)?;
        let source = self.get_shard(source_shard_id)?;
        let target_info = self.shard_health_blocking(&target)?;
        let source_info = self.shard_health_blocking(&source)?;

        let new_start = target_info.range_start.min(source_info.range_start);
        let new_end = target_info.range_end.max(source_info.range_end);

        // Extend the target's range to the union; mark source as
        // `Retiring` (ADR-034 post-cutover state).
        self.set_shard_range_blocking(&target, new_start, new_end)?;
        self.set_shard_state_blocking(&source, ShardState::Retiring)?;
        Ok(())
    }

    async fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.register_consumer(consumer, position).await
    }

    async fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.advance_watermark(consumer, position).await
    }
}

/// ADR-033 §3 step 3: replay upper-half deltas from `source` into
/// `new_store`. Reads in batches of `BATCH_SIZE` from the source's
/// log, filters by `hashed_key ∈ [upper_start, upper_end)`, and
/// appends each filtered delta to the new shard via Raft consensus.
///
/// Returns the count of deltas replayed (for the
/// `kiseki_log_split_replayed_total` metric — wired in a follow-up).
///
/// Sequence numbers are NOT preserved across shards: each shard's
/// tip is independent. The replayed delta gets a fresh sequence
/// from the new shard's Raft log. Composition-level identity
/// (`chunk_refs`, `payload`, `hashed_key`, `tenant_id`, `operation`,
/// `timestamp`) is preserved.
async fn redistribute_upper_half(
    source: &Arc<OpenRaftLogStore>,
    new_store: &Arc<OpenRaftLogStore>,
    upper_start: [u8; 32],
    upper_end: [u8; 32],
) -> Result<u64, LogError> {
    use crate::traits::{AppendDeltaRequest, ReadDeltasRequest};

    /// Read up to this many deltas per `read_deltas` call.
    const BATCH_SIZE: u64 = 1024;

    let source_info = source.shard_health().await;
    let source_id = source_info.shard_id;
    let tip = source_info.tip.0;
    if tip == 0 {
        return Ok(0); // empty source — nothing to redistribute
    }

    let mut replayed: u64 = 0;
    let mut from = 1u64;
    while from <= tip {
        let to = (from + BATCH_SIZE - 1).min(tip);
        let deltas = source
            .read_deltas(ReadDeltasRequest {
                shard_id: source_id,
                from: SequenceNumber(from),
                to: SequenceNumber(to),
            })
            .await?;
        for delta in deltas {
            // Range filter: only deltas in [upper_start, upper_end).
            if delta.header.hashed_key < upper_start || delta.header.hashed_key >= upper_end {
                continue;
            }
            new_store
                .append_delta(AppendDeltaRequest {
                    shard_id: new_store.shard_id(),
                    tenant_id: delta.header.tenant_id,
                    operation: delta.header.operation,
                    timestamp: delta.header.timestamp.clone(),
                    hashed_key: delta.header.hashed_key,
                    chunk_refs: delta.header.chunk_refs.clone(),
                    payload: delta.payload.ciphertext.clone(),
                    has_inline_data: delta.header.has_inline_data,
                })
                .await?;
            replayed = replayed.saturating_add(1);
        }
        from = to + 1;
    }
    tracing::info!(
        source = %source_id.0,
        new_shard = %new_store.shard_id().0,
        replayed,
        "split: upper-half delta redistribution complete (ADR-033 §3)",
    );
    Ok(replayed)
}
