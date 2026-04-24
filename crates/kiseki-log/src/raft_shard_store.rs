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

/// Run a future on the dedicated Raft runtime from any context.
///
/// Spawns the work on the Raft runtime via `spawn` and blocks the
/// current thread waiting for the result via a oneshot channel.
/// This avoids both:
/// - "cannot start a runtime from within a runtime" (`block_on`)
/// - Worker thread starvation (`block_in_place` with 32+ concurrent requests)
fn run_on_raft<F, T>(rt: &tokio::runtime::Runtime, f: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    rt.spawn(async move {
        tracing::trace!("run_on_raft: future starting");
        let result = f.await;
        tracing::trace!("run_on_raft: future completed, sending result");
        let _ = tx.send(result);
    });
    tracing::trace!("run_on_raft: waiting for result on mpsc channel");
    rx.recv()
        .expect("Raft runtime task dropped without completing")
}

impl LogOps for RaftShardStore {
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(req.shard_id)?;
        run_on_raft(&self.rt, async move { store.append_delta(req).await })
    }

    fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let store = self.get_shard(req.shard_id)?;
        run_on_raft(&self.rt, async move { store.read_deltas(req).await })
    }

    fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let store = self.get_shard(shard_id)?;
        let info = run_on_raft(&self.rt, async move { store.shard_health().await });
        Ok(info)
    }

    fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        let store = self.get_shard(shard_id)?;
        run_on_raft(
            &self.rt,
            async move { store.set_maintenance(enabled).await },
        )
    }

    fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let store = self.get_shard(shard_id)?;
        run_on_raft(&self.rt, async move { store.truncate_log().await })
    }

    fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        let store = self.get_shard(shard_id)?;
        run_on_raft(&self.rt, async move { store.compact_shard().await })
    }
}
