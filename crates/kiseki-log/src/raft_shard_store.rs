//! Raft-backed shard store for multi-node clusters.
//!
//! Wraps per-shard `OpenRaftLogStore` instances behind the sync
//! `LogOps` trait. Each shard gets its own Raft group for independent
//! consensus. The sync↔async bridge uses `block_in_place` (safe on
//! tokio's multi-thread runtime).
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
use crate::shard::{ShardConfig, ShardInfo};
use crate::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};

/// Raft-backed shard store for multi-node clusters.
///
/// Holds a map of `ShardId → OpenRaftLogStore`. Each shard has its
/// own Raft group with independent leader election. The `LogOps`
/// trait methods bridge sync→async via `block_on` on a **dedicated**
/// tokio runtime, separate from the server's main runtime.
///
/// This avoids deadlocks when the S3/NFS gateway calls `append_delta`
/// from async context: `block_in_place` + `block_on` on the *same*
/// runtime would starve worker threads under concurrent load.
///
/// When `data_dir` is set, uses `RedbRaftLogStore` for persistent
/// Raft state (Phase 12b). When `None`, uses in-memory `MemLogStore`.
pub struct RaftShardStore {
    shards: Mutex<HashMap<ShardId, Arc<OpenRaftLogStore>>>,
    node_id: u64,
    peers: BTreeMap<u64, String>,
    /// Dedicated runtime for Raft async operations. Separate from the
    /// server's main runtime to prevent deadlocks when sync gateway
    /// code calls into async Raft consensus.
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
    pub fn new(
        node_id: u64,
        peers: BTreeMap<u64, String>,
        data_dir: Option<PathBuf>,
    ) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .thread_name("kiseki-raft")
            .enable_all()
            .build()
            .expect("failed to create Raft runtime");
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

        let store = self.rt.block_on(async {
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
            if let Some(addr) = raft_addr {
                std::mem::drop(store.spawn_rpc_server(addr.to_owned()));
                tracing::info!(shard_id = %shard_id.0, addr, "Raft RPC server started for shard");
            }

            Arc::new(store)
        });

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

impl LogOps for RaftShardStore {
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.shard_id)?;
        self.rt.block_on(store.append_delta(req))
    }

    fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let store = self.get_shard(req.shard_id)?;
        self.rt.block_on(store.read_deltas(req))
    }

    fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let store = self.get_shard(shard_id)?;
        let info = self.rt.block_on(store.shard_health());
        Ok(info)
    }

    fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        self.rt.block_on(store.set_maintenance(enabled))
    }

    fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(shard_id)?;
        self.rt.block_on(store.truncate_log())
    }

    fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        let store = self.get_shard(shard_id)?;
        self.rt.block_on(store.compact_shard())
    }
}
