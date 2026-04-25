//! Raft-backed shard store for multi-node clusters.
//!
//! Wraps per-shard `OpenRaftLogStore` instances behind the async
//! `LogOps` trait (ADR-032). Each shard gets its own Raft group for
//! independent consensus. Methods are called directly from async
//! context — no sync↔async bridge needed.
//!
//! Phase I2: multi-node Raft consensus with in-memory Raft log
//! (`MemLogStore`). Durability via Raft replication to majority.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};

use crate::delta::Delta;
use crate::error::LogError;
use crate::raft::OpenRaftLogStore;
use crate::shard::{ShardConfig, ShardInfo, ShardState};
use crate::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};

/// Raft-backed shard store for multi-node clusters.
///
/// Holds a map of `ShardId → OpenRaftLogStore`. Each shard has its
/// own Raft group with independent leader election. The `LogOps`
/// trait methods are async (ADR-032), so callers await directly
/// without sync↔async bridging.
///
/// When `data_dir` is set, uses `RedbRaftLogStore` for persistent
/// Raft state (Phase 12b). When `None`, uses in-memory `MemLogStore`.
pub struct RaftShardStore {
    shards: Mutex<HashMap<ShardId, Arc<OpenRaftLogStore>>>,
    node_id: u64,
    peers: BTreeMap<u64, String>,
    /// Dedicated runtime for Raft async operations. Kept separate from
    /// the server's main runtime so NFS/FUSE threads can call `block_on`
    /// without nesting, and for Raft RPC server + bootstrap.
    rt: tokio::runtime::Runtime,
    data_dir: Option<PathBuf>,
    inline_store: Option<Arc<dyn kiseki_common::inline_store::InlineStore>>,
}

impl RaftShardStore {
    /// Create a new (empty) Raft shard store.
    ///
    /// Spawns a dedicated tokio runtime for Raft async operations.
    /// When `data_dir` is `Some`, Raft log state is persisted to redb
    /// and survives restart. When `None`, uses in-memory log (volatile).
    #[must_use]
    pub fn new(node_id: u64, peers: BTreeMap<u64, String>, data_dir: Option<PathBuf>) -> Self {
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
            node_id,
            peers,
            rt,
            data_dir,
            inline_store: None,
        }
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

    /// Create a shard with its own Raft group.
    ///
    /// When `bootstrap` is true, calls `raft.initialize()` with the
    /// configured peers (seed node). When false, the node joins the
    /// existing cluster by receiving membership from the leader.
    ///
    /// Optionally spawns the Raft RPC server on `raft_addr`.
    ///
    /// # Panics
    ///
    /// Panics if the Raft instance fails to initialize (fatal for
    /// server startup).
    pub fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        _node_id: NodeId,
        _config: ShardConfig,
        raft_addr: Option<&str>,
        bootstrap: bool,
    ) {
        let peers = self.peers.clone();
        let node_id = self.node_id;
        let data_dir = self.data_dir.clone();
        let inline_store = self.inline_store.clone();

        let raft_addr_owned = raft_addr.map(str::to_owned);
        let handle = self.rt.handle().clone();
        let store = std::thread::spawn(move || {
            handle.block_on(async {
                let store = if bootstrap {
                    OpenRaftLogStore::new(
                        node_id,
                        shard_id,
                        tenant_id,
                        &peers,
                        data_dir.as_deref(),
                        inline_store,
                    )
                    .await
                    .expect("failed to create Raft log store (seed)")
                } else {
                    OpenRaftLogStore::new_follower(
                        node_id,
                        shard_id,
                        tenant_id,
                        &peers,
                        data_dir.as_deref(),
                        inline_store,
                    )
                    .await
                    .expect("failed to create Raft log store (follower)")
                };

                // Spawn RPC server for this shard's Raft group.
                if let Some(addr) = raft_addr_owned {
                    tracing::info!(shard_id = %shard_id.0, %addr, "Raft RPC server started for shard");
                    std::mem::drop(store.spawn_rpc_server(addr));
                }

                Arc::new(store)
            })
        })
        .join()
        .expect("Raft shard creation thread panicked");

        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shards.insert(shard_id, store);
    }

    /// Look up a shard's Raft store.
    fn get_shard(&self, shard_id: ShardId) -> Result<Arc<OpenRaftLogStore>, LogError> {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shards
            .get(&shard_id)
            .cloned()
            .ok_or(LogError::ShardNotFound(shard_id))
    }
}

#[async_trait::async_trait]
impl LogOps for RaftShardStore {
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.shard_id)?;
        store.append_delta(req).await
    }

    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let store = self.get_shard(req.shard_id)?;
        store.read_deltas(req).await
    }

    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let store = self.get_shard(shard_id)?;
        let info = store.shard_health().await;
        Ok(info)
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
        // For RaftShardStore, create a new per-shard Raft store.
        // Simplified: just create through the inner store mechanism.
        let _ = (shard_id, tenant_id, node_id, config);
        // Full implementation would create a Raft group here.
    }

    fn update_shard_range(&self, shard_id: ShardId, _range_start: [u8; 32], _range_end: [u8; 32]) {
        // Raft shard range updates go through the control plane Raft group,
        // not the shard's Raft group. This is a local metadata update.
        let _ = shard_id;
    }

    fn set_shard_state(&self, shard_id: ShardId, _state: ShardState) {
        // Shard state transitions are coordinated by the control plane.
        let _ = shard_id;
    }

    async fn register_consumer(&self, shard_id: ShardId, consumer: &str, position: SequenceNumber) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.register_consumer(consumer, position).await
    }

    async fn advance_watermark(&self, shard_id: ShardId, consumer: &str, position: SequenceNumber) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        store.advance_watermark(consumer, position).await
    }
}
