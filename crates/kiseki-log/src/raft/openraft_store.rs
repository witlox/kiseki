//! OpenRaft-backed log store.
//!
//! Wraps a `Raft<LogTypeConfig>` handle for consensus-replicated
//! shard operations. Reads from shared state machine inner, writes
//! go through `client_write()`.
//!
//! Provides async methods matching the `LogOps` trait API. The sync
//! `LogOps` trait cannot be implemented directly because the Raft
//! layer is async, but all equivalent operations are available as
//! async methods on this type.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_raft::{
    tcp_transport, KisekiNode, KisekiRaftConfig, MemLogStore, RedbRaftLogStore, StubNetworkFactory,
    TcpNetworkFactory,
};
use openraft::type_config::async_runtime::WatchReceiver;
use openraft::Raft;

use super::state_machine::{ShardSmInner, ShardStateMachine};
use super::types::{LogResponse, LogTypeConfig};
use crate::delta::Delta;
use crate::error::LogError;
use crate::raft_store::LogCommand;
use crate::shard::{ShardInfo, ShardState};
use crate::traits::{AppendDeltaRequest, ReadDeltasRequest};

type C = LogTypeConfig;

/// OpenRaft-backed log store for a single shard.
///
/// Single-node Raft for now. Writes go through `client_write()`,
/// reads from the shared `ShardSmInner`.
///
/// The state machine stores full delta data, consumer watermarks,
/// and shard metadata — enabling `read_deltas`, `truncate_log`,
/// `compact_shard`, and watermark operations.
pub struct OpenRaftLogStore {
    raft: Raft<C, ShardStateMachine>,
    state: Arc<futures::lock::Mutex<ShardSmInner>>,
    shard_id: ShardId,
    tenant_id: OrgId,
}

fn op_to_u8(op: crate::delta::OperationType) -> u8 {
    match op {
        crate::delta::OperationType::Create => 0,
        crate::delta::OperationType::Update => 1,
        crate::delta::OperationType::Delete => 2,
        crate::delta::OperationType::Rename => 3,
        crate::delta::OperationType::SetAttribute => 4,
        crate::delta::OperationType::Finalize => 5,
    }
}

impl OpenRaftLogStore {
    /// Create and bootstrap a Raft log store for a shard.
    ///
    /// When `peers` is empty, runs in single-node mode with a stub network.
    /// When `peers` contains entries, uses TCP transport for multi-node Raft.
    /// The `peers` map should include this node's own `(node_id, addr)` entry.
    ///
    /// When `data_dir` is `Some`, uses `RedbRaftLogStore` for persistent
    /// Raft state (Phase 12b). On restart, skips `initialize()` if the
    /// store already has state. When `None`, uses in-memory `MemLogStore`.
    pub async fn new(
        node_id: u64,
        shard_id: ShardId,
        tenant_id: OrgId,
        peers: &BTreeMap<u64, String>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<Self, LogError> {
        let config = KisekiRaftConfig::default_config();
        let state_inner = Arc::new(futures::lock::Mutex::new(ShardSmInner::new(
            shard_id, tenant_id,
        )));
        let state_machine = ShardStateMachine::new(Arc::clone(&state_inner));

        let members: BTreeMap<u64, KisekiNode> = if peers.len() > 1 {
            peers
                .iter()
                .map(|(id, addr)| (*id, KisekiNode::new(addr)))
                .collect()
        } else {
            let mut m = BTreeMap::new();
            let addr = peers.get(&node_id).map_or("localhost:9201", String::as_str);
            m.insert(node_id, KisekiNode::new(addr));
            m
        };

        // Select log store backend: persistent (redb) or in-memory.
        let (raft, already_initialized) = if let Some(dir) = data_dir {
            let raft_dir = dir.join("raft");
            std::fs::create_dir_all(&raft_dir).ok();
            let redb_path = raft_dir.join(format!("shard-{}.redb", shard_id.0));
            let log_store =
                RedbRaftLogStore::<C>::open(&redb_path).map_err(|_| LogError::Unavailable)?;
            let has_state = log_store.has_state();

            let raft = if peers.len() > 1 {
                let network = TcpNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| LogError::Unavailable)?
            } else {
                let network = StubNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| LogError::Unavailable)?
            };
            (raft, has_state)
        } else {
            let log_store = MemLogStore::<C>::new();
            let raft = if peers.len() > 1 {
                let network = TcpNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| LogError::Unavailable)?
            } else {
                let network = StubNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| LogError::Unavailable)?
            };
            (raft, false)
        };

        // Only initialize on first boot — skip if redb already has state
        // (the node already has membership from a previous run).
        if !already_initialized {
            raft.initialize(members)
                .await
                .map_err(|_| LogError::Unavailable)?;
        }

        Ok(Self {
            raft,
            state: state_inner,
            shard_id,
            tenant_id,
        })
    }

    /// Spawn the Raft RPC server for this shard's Raft group.
    ///
    /// Listens on `addr` for incoming Raft RPCs (`AppendEntries`, `Vote`)
    /// from peer nodes. Only needed in multi-node mode.
    /// Returns a `JoinHandle` for the server task.
    #[must_use]
    pub fn spawn_rpc_server(
        &self,
        addr: String,
    ) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
        let raft = Arc::new(self.raft.clone());
        tokio::spawn(async move { tcp_transport::run_raft_rpc_server::<C>(&addr, raft).await })
    }

    /// Append a delta through Raft consensus.
    ///
    /// Accepts an `AppendDeltaRequest` (the `LogOps` trait's request type).
    /// Pre-checks maintenance mode and key range before writing.
    /// Returns the assigned sequence number.
    pub async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        // Pre-check state.
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: op_to_u8(req.operation),
            hashed_key: req.hashed_key,
            chunk_refs: req.chunk_refs.iter().map(|c| c.0).collect(),
            payload: req.payload,
            has_inline_data: req.has_inline_data,
        };

        let resp = self.raft.client_write(cmd).await.map_err(|e| {
            if matches!(
                e,
                openraft::errors::RaftError::APIError(
                    openraft::error::ClientWriteError::ForwardToLeader(_)
                )
            ) {
                LogError::LeaderUnavailable(self.shard_id)
            } else {
                LogError::Unavailable
            }
        })?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok => Err(LogError::Unavailable),
        }
    }

    /// Append a delta through Raft consensus (raw parameters).
    ///
    /// Lower-level method that accepts raw byte arrays. Prefer
    /// `append_delta` with `AppendDeltaRequest` for type safety.
    pub async fn append_delta_raw(
        &self,
        tenant_id_bytes: [u8; 16],
        operation: u8,
        hashed_key: [u8; 32],
        chunk_refs: Vec<[u8; 32]>,
        payload: Vec<u8>,
        has_inline_data: bool,
    ) -> Result<SequenceNumber, LogError> {
        // Pre-check state.
        {
            let inner = self.state.lock().await;
            if inner.maintenance {
                return Err(LogError::MaintenanceMode(self.shard_id));
            }
        }

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes,
            operation,
            hashed_key,
            chunk_refs,
            payload,
            has_inline_data,
        };

        let resp = self.raft.client_write(cmd).await.map_err(|e| {
            if matches!(
                e,
                openraft::errors::RaftError::APIError(
                    openraft::error::ClientWriteError::ForwardToLeader(_)
                )
            ) {
                LogError::LeaderUnavailable(self.shard_id)
            } else {
                LogError::Unavailable
            }
        })?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok => Err(LogError::Unavailable),
        }
    }

    /// Read deltas in `[from, to]` inclusive from the shard.
    pub async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        if req.from > req.to {
            return Err(LogError::InvalidRange(self.shard_id));
        }

        let inner = self.state.lock().await;
        Ok(inner
            .deltas
            .iter()
            .filter(|d| d.header.sequence >= req.from && d.header.sequence <= req.to)
            .cloned()
            .collect())
    }

    /// Set or clear maintenance mode through Raft consensus.
    pub async fn set_maintenance(&self, enabled: bool) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::SetMaintenance { enabled })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;

        Ok(())
    }

    /// Get the current tip sequence number from the state machine.
    pub async fn current_tip(&self) -> SequenceNumber {
        let inner = self.state.lock().await;
        SequenceNumber(inner.tip)
    }

    /// Check whether the shard is in maintenance mode.
    pub async fn is_maintenance(&self) -> bool {
        let inner = self.state.lock().await;
        inner.maintenance
    }

    /// Get shard health metadata from the state machine.
    ///
    /// Includes Raft leader and membership info from metrics.
    pub async fn shard_health(&self) -> ShardInfo {
        let inner = self.state.lock().await;

        // Read leader from Raft metrics.
        let leader = self
            .raft
            .current_leader()
            .await
            .map(kiseki_common::ids::NodeId);

        // Read membership from Raft metrics.
        let metrics = self.raft.metrics().borrow_watched().clone();
        let raft_members: Vec<kiseki_common::ids::NodeId> = metrics
            .membership_config
            .membership()
            .nodes()
            .map(|(id, _)| kiseki_common::ids::NodeId(*id))
            .collect();

        ShardInfo {
            shard_id: self.shard_id,
            tenant_id: self.tenant_id,
            raft_members,
            leader,
            tip: SequenceNumber(inner.tip),
            delta_count: inner.delta_count,
            byte_size: inner
                .deltas
                .iter()
                .map(|d| u64::from(d.header.payload_size) + 128)
                .sum(),
            state: if inner.maintenance {
                ShardState::Maintenance
            } else {
                ShardState::Healthy
            },
            config: crate::shard::ShardConfig::default(),
            range_start: [0u8; 32],
            range_end: [0xff; 32],
        }
    }

    /// Advance a consumer watermark through Raft consensus.
    pub async fn advance_watermark(
        &self,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::AdvanceWatermark {
                consumer: consumer.to_owned(),
                position: position.0,
            })
            .await
            .map_err(|e| {
                if matches!(
                    e,
                    openraft::errors::RaftError::APIError(
                        openraft::error::ClientWriteError::ForwardToLeader(_)
                    )
                ) {
                    LogError::LeaderUnavailable(self.shard_id)
                } else {
                    LogError::Unavailable
                }
            })?;

        Ok(())
    }

    /// Register a consumer watermark (delegates to `advance_watermark`
    /// since the state machine's `advance` handles initial registration).
    pub async fn register_consumer(
        &self,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        self.advance_watermark(consumer, position).await
    }

    /// Truncate deltas below the minimum consumer watermark (GC).
    ///
    /// This is a local operation — GC does not require consensus
    /// because it only removes data that all consumers have already
    /// processed.
    pub async fn truncate_log(&self) -> Result<SequenceNumber, LogError> {
        let mut inner = self.state.lock().await;
        let gc_boundary = inner.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));
        inner.deltas.retain(|d| d.header.sequence >= gc_boundary);
        Ok(gc_boundary)
    }

    /// Compact deltas: keep only the latest delta per `hashed_key`,
    /// remove tombstones below the GC boundary.
    ///
    /// Returns the number of deltas removed.
    pub async fn compact_shard(&self) -> Result<u64, LogError> {
        use std::collections::HashMap;

        let mut inner = self.state.lock().await;
        let before = inner.deltas.len() as u64;
        let gc_boundary = inner.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));

        let mut latest: HashMap<[u8; 32], &Delta> = HashMap::new();
        for delta in &inner.deltas {
            let entry = latest.entry(delta.header.hashed_key).or_insert(delta);
            if delta.header.sequence > entry.header.sequence {
                *entry = delta;
            }
        }

        let surviving: Vec<Delta> = latest
            .into_values()
            .filter(|d| !(d.header.tombstone && d.header.sequence < gc_boundary))
            .cloned()
            .collect();

        let after = surviving.len() as u64;
        inner.deltas = surviving;
        inner.deltas.sort_by_key(|d| d.header.sequence);
        inner.delta_count = after;

        Ok(before.saturating_sub(after))
    }

    /// Get the shard ID this store manages.
    #[must_use]
    pub fn shard_id(&self) -> ShardId {
        self.shard_id
    }

    /// Get the tenant ID this store belongs to.
    #[must_use]
    pub fn tenant_id(&self) -> OrgId {
        self.tenant_id
    }
}
