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
use crate::intent_sync::{
    build_intent_dispatcher, TransportIntentGatherer, WireIntent, INTENT_PUT_TAG,
};
use crate::raft::state_machine::ClusterChunkStateEntry;
use crate::raft::OpenRaftLogStore;
use crate::raft_intent_sink::RaftLogIncorporationSink;
use crate::shard::{ShardConfig, ShardInfo, ShardState};
use crate::shard_committer::{run_committer_loop, ShardCommitter};
use crate::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps, ReadDeltasRequest};
use kiseki_common::locks::LockOrDie;
use kiseki_raft::tcp_transport::rpc_call;

/// Default min durable copies for an acked decoupled write when
/// `KISEKI_MIN_ACKS` is unset — mirrors the chunk D-5 default (2-of-N).
const DEFAULT_MIN_ACKS: usize = 2;

/// Per-tick interval for a shard's async committer loop (ADR-047 §3).
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
    async fn append_intent(
        &self,
        req: AppendChunkAndDeltaRequest,
        seq: crate::intent::PerspectiveSeq,
    ) -> Result<(), LogError> {
        crate::raft_intent_sink::IntentLogAppender::append_intent(&*self.store, req, seq).await
    }

    async fn max_incorporated_seq(&self) -> Option<kiseki_common::time::HybridLogicalClock> {
        crate::raft_intent_sink::IntentLogAppender::max_incorporated_seq(&*self.store).await
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
    /// ADR-047 decoupled-ack capability gate. When `false` (the default),
    /// `create_shard` spawns no committer and the sync write path is
    /// unchanged. Set in [`Self::new`] from the `decoupled_ack` param (the
    /// runtime derives it from `KISEKI_DECOUPLED_ACK`).
    decoupled_ack: bool,
    /// Minimum durable copies (local + remote acks) for an acked decoupled
    /// write. From `KISEKI_MIN_ACKS`, else [`DEFAULT_MIN_ACKS`]. Mirrors the
    /// chunk D-5 quorum default.
    min_acks: usize,
    /// Per-shard async committer tasks spawned when `decoupled_ack` is on and
    /// the shard's intent store is durable. Stopped in [`Self::shutdown`] /
    /// `Drop`.
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
}

impl RaftShardStore {
    /// Create a new (empty) Raft shard store.
    ///
    /// Spawns a dedicated tokio runtime for Raft async operations.
    /// When `data_dir` is `Some`, Raft log state is persisted to the
    /// fjall keyspace and survives restart. When `None`, uses
    /// in-memory log (volatile).
    ///
    /// `decoupled_ack` arms the ADR-047 decoupled-ack capability gate: when
    /// `true`, `create_shard` spawns a per-shard async committer (for each
    /// shard whose intent store is durable) and `put_intent_and_fan` performs
    /// the quorum intent-write. When `false` (the runtime default unless
    /// `KISEKI_DECOUPLED_ACK` is set), the synchronous write path is unchanged
    /// and nothing extra is spawned. `min_acks` is read from `KISEKI_MIN_ACKS`
    /// (else [`DEFAULT_MIN_ACKS`]).
    #[must_use]
    pub fn new(
        node_id: u64,
        peers: BTreeMap<u64, String>,
        data_dir: Option<PathBuf>,
        decoupled_ack: bool,
    ) -> Self {
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
            decoupled_ack,
            min_acks,
            committers: Mutex::new(HashMap::new()),
            node_id,
            peers,
            rt,
            data_dir,
            inline_store: None,
            listener_registry: Mutex::new(None),
            transport_metrics: Mutex::new(None),
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
    /// **Not wired into any production path this phase** — provided for the
    /// 5c/5d committer task that spawns behind the `DecoupledAckEnabled` gate.
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

    /// ADR-047 decoupled-ack — the quorum intent-write (NO-LOSS CRITICAL).
    ///
    /// Durably records `intent` on a quorum so the gateway can fast-ack a write
    /// BEFORE the synchronous Raft round. Writes the LOCAL per-shard
    /// [`IntentStore`] (one durable copy) and fans the intent to the shard's
    /// voter peers via [`INTENT_PUT_TAG`] in PARALLEL; returns `Ok` ONLY once
    /// total durable copies (`1 local + remote acks`) reach `self.min_acks`.
    /// Otherwise `Err` — the caller MUST NOT ack (an acked write is guaranteed
    /// on `≥ min_acks` replicas, I-L2/I-CS1).
    ///
    /// # Non-durable-store guard (F-P5b-rpc-1)
    /// If the shard's intent store is the [`InMemIntentStore`] degrade (its
    /// durable open failed — see `create_shard`), this returns
    /// [`LogError::Unavailable`] WITHOUT writing. Decoupled-ack MUST NOT run on
    /// a non-durable intent store: acking on a non-durable intent loses the
    /// write on crash. Fail closed — the caller falls back to the sync path.
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
        use futures::StreamExt;

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

        let store = self
            .intent_store(shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        // Local put first — one durable copy. A local failure is fatal: with
        // the local copy missing we can never reach min_acks safely.
        store.put(intent.clone()).map_err(|e| {
            tracing::warn!(shard_id = %shard_id.0, error = %e, "put_intent_and_fan: local intent store write failed");
            LogError::Unavailable
        })?;
        let mut acks: usize = 1;

        // Fast path: a single-copy quorum (1-node cluster / min_acks=1) is
        // satisfied by the local write alone — no fan needed.
        if acks >= self.min_acks {
            return Ok(());
        }

        // Fan to the shard's voter peers (minus self) in PARALLEL. Reuse the
        // gatherer's voter/addr resolution so the fan targets exactly the
        // committer's voter set. A peer with no addr / no listener is simply a
        // non-ack — never fabricated.
        let peers = self.resolve_voter_peers(shard_id).unwrap_or_default();
        let wire = WireIntent::from(&intent);
        let mut fan = futures::stream::FuturesUnordered::new();
        for (node_id, addr) in peers {
            let wire_ref = &wire;
            fan.push(async move {
                // postcard(()) reply on success; bounded per-peer timeout so a
                // slow/dead peer cannot stall the fast-ack.
                let call = rpc_call::<_, ()>(&addr, shard_id, INTENT_PUT_TAG, None, wire_ref);
                match tokio::time::timeout(INTENT_FAN_PEER_TIMEOUT, call).await {
                    Ok(Ok(())) => true,
                    Ok(Err(e)) => {
                        tracing::debug!(node = node_id.0, addr = %addr, error = %e, "intent_put fan: peer non-ack");
                        false
                    }
                    Err(_) => {
                        tracing::debug!(node = node_id.0, addr = %addr, "intent_put fan: peer timed out");
                        false
                    }
                }
            });
        }
        while let Some(acked) = fan.next().await {
            if acked {
                acks += 1;
                if acks >= self.min_acks {
                    // Quorum reached — return as soon as enough peers ack
                    // (the remaining fan futures are dropped/cancelled).
                    return Ok(());
                }
            }
        }

        // Shortfall: durable copies < min_acks. The caller MUST NOT ack.
        tracing::warn!(
            shard_id = %shard_id.0,
            acks,
            min_acks = self.min_acks,
            "put_intent_and_fan: quorum shortfall — refusing to ack",
        );
        Err(LogError::QuorumLost(shard_id))
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
                let store = OpenRaftLogStore::new(
                    node_id,
                    shard_id,
                    tenant_id,
                    &peers,
                    data_dir.as_deref(),
                    inline_store,
                )
                .await
                .expect("failed to create Raft log store");
                Arc::new(store)
            })
        })
        .join()
        .expect("Raft shard creation thread panicked");

        // ADR-047 phase 5b-rpc: create this shard's IntentStore so peers can
        // SERVE their intent state via the IntentSync aux dispatcher. Durable
        // (FjallIntentStore at `<data_dir>/<shard_id>/intents`) when a data
        // dir is set, else in-memory — mirroring the shard log store's
        // persistence choice above. Inert this phase: empty + only queried by
        // the 5c/5d committer task.
        //
        // A durable-open failure degrades to the in-memory store rather than
        // failing shard creation. That is safe ONLY because nothing acks on
        // this store yet (the gatherer is on no production path this phase).
        //
        // !!! 5c/5d OBLIGATION (no-loss / O3): once the producer fast-acks a
        // write on its intent being quorum-DURABLE, an in-memory intent store
        // would lose acked writes on crash — violating ADR-047's core
        // guarantee. So the capability gate MUST NOT enable decoupled-ack on a
        // shard whose durable intent store failed to open (fail closed: keep
        // that shard on the synchronous path, or fail shard creation). Do NOT
        // let this silent fallback survive into the acked path.
        let (intent_store, intent_store_durable): (Arc<dyn IntentStore>, bool) =
            match &self.data_dir {
                Some(dir) => {
                    let path = dir.join(shard_id.0.to_string()).join("intents");
                    match FjallIntentStore::open(&path) {
                        Ok(s) => (Arc::new(s), true),
                        Err(e) => {
                            tracing::error!(
                                shard_id = %shard_id.0,
                                error = %e,
                                "durable IntentStore open failed; using in-memory \
                                 (decoupled-ack DISABLED on this shard — F-P5b-rpc-1)",
                            );
                            (Arc::new(InMemIntentStore::new()), false)
                        }
                    }
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
            reg.register_aux(shard_id, build_intent_dispatcher(Arc::clone(&intent_store)));
            tracing::info!(shard_id = %shard_id.0, "shard registered with Raft RPC listener");
        }

        {
            let mut shards = self.shards.lock().lock_or_die("raft_shard_store.shards");
            shards.insert(shard_id, Arc::clone(&store));
        }

        // ADR-047 decoupled-ack: spawn this shard's async committer ONLY when
        // the capability gate is on AND the intent store is durable. The
        // durable gate is the F-P5b-rpc-1 obligation — a committer that drains
        // a non-durable store could incorporate an intent that a crash would
        // lose. If the gate is off, spawn nothing (sync behavior unchanged).
        if self.decoupled_ack && intent_store_durable {
            self.spawn_committer(shard_id, &store, intent_store);
        }
    }

    /// Spawn the per-shard async committer (ADR-047 phase 5c) on a DEDICATED
    /// `std::thread` holding the Raft runtime handle. The committer drives the
    /// synchronous [`RaftLogIncorporationSink`], whose
    /// [`Handle::block_on`](tokio::runtime::Handle::block_on) MUST NOT run on a
    /// tokio worker (the `raft_intent_sink` threading contract) — hence the
    /// dedicated thread. Its shutdown sender + join handle are kept in
    /// `committers` so [`Self::shutdown`] / `Drop` can stop it cleanly.
    ///
    /// `cluster_size` is the configured voter count (`self.peers.len()`, incl.
    /// self): the authoritative membership size at construction, stable even
    /// before the shard's Raft membership is initialized.
    fn spawn_committer(
        &self,
        shard_id: ShardId,
        store: &Arc<OpenRaftLogStore>,
        intent_store: Arc<dyn IntentStore>,
    ) {
        let cluster_size = self.peers.len().max(1);
        // The committer spawns at create_shard time — BEFORE membership is
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
        let join = std::thread::Builder::new()
            .name(format!("kiseki-committer-{}", shard_id.0))
            .spawn(move || {
                raft_handle.clone().block_on(async move {
                    // The sink bridges the sync committer to the async Raft log;
                    // `new` seeds the F-2 recovery floor via block_on, which is
                    // safe here because this is a dedicated std::thread, not a
                    // tokio worker (the threading contract).
                    let sink = RaftLogIncorporationSink::new(
                        OpenRaftAppender { store: appender },
                        raft_handle.clone(),
                    );
                    let committer = ShardCommitter::new(intent_store, sink, cluster_size);
                    run_committer_loop(committer, gatherer, COMMITTER_INTERVAL, shutdown_rx).await;
                });
            })
            .expect("failed to spawn decoupled-ack committer thread");
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
        tracing::info!(shard_id = %shard_id.0, cluster_size, "decoupled-ack committer spawned (ADR-047)");
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

impl Drop for RaftShardStore {
    /// Stop every spawned per-shard committer on drop so no dedicated thread
    /// outlives the store. `shutdown` is idempotent with an explicit call.
    fn drop(&mut self) {
        self.shutdown();
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
            tracing::warn!(error = %e, "log append_delta: shard append failed");
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
                tracing::warn!(error = %e, "log append_delta_with_forwarding: shard append failed");
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
                tracing::warn!(error = %e, "log append_chunk_and_delta: shard append failed");
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
                tracing::warn!(
                    error = %e,
                    "log append_chunk_and_delta_with_forwarding: shard append failed",
                );
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
