//! OpenRaft-backed log store.
//!
//! Wraps a `Raft<LogTypeConfig>` handle for consensus-replicated
//! shard operations. Reads from shared state machine inner, writes
//! go through `client_write()`.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_common::ids::SequenceNumber;
use kiseki_raft::{KisekiNode, KisekiRaftConfig, MemLogStore, StubNetworkFactory};
use openraft::Raft;

use super::state_machine::{ShardSmInner, ShardStateMachine};
use super::types::{LogResponse, LogTypeConfig};
use crate::error::LogError;
use crate::raft_store::LogCommand;
use crate::shard::ShardInfo;

type C = LogTypeConfig;

/// OpenRaft-backed log store.
///
/// Single-node Raft for now. Writes go through `client_write()`,
/// reads from the shared `ShardSmInner`.
///
/// Note: `ShardSmInner` only tracks `delta_count`, `tip`, and
/// `maintenance` — not actual delta data. Delta data lives in the
/// Raft log entries. This is by design for the state machine layer.
pub struct OpenRaftLogStore {
    raft: Raft<C, ShardStateMachine>,
    state: Arc<futures::lock::Mutex<ShardSmInner>>,
}

impl OpenRaftLogStore {
    /// Create and bootstrap a single-node Raft log store.
    pub async fn new(node_id: u64) -> Result<Self, LogError> {
        let config = KisekiRaftConfig::default_config();
        let log_store = MemLogStore::<C>::new();
        let state_inner = Arc::new(futures::lock::Mutex::new(ShardSmInner::new()));
        let state_machine = ShardStateMachine::new(Arc::clone(&state_inner));
        let network = StubNetworkFactory::<C>::new();

        let raft = Raft::new(node_id, config, network, log_store, state_machine)
            .await
            .map_err(|_e| LogError::Unavailable)?;

        // Initialize as single-node cluster.
        let mut members = BTreeMap::new();
        members.insert(node_id, KisekiNode::new("localhost:9201"));
        raft.initialize(members)
            .await
            .map_err(|_| LogError::Unavailable)?;

        Ok(Self {
            raft,
            state: state_inner,
        })
    }

    /// Append a delta through Raft consensus.
    ///
    /// Constructs a `LogCommand::AppendDelta` and sends it through
    /// `client_write`. Returns the assigned sequence number (tip).
    pub async fn append_delta(
        &self,
        tenant_id_bytes: [u8; 16],
        operation: u8,
        hashed_key: [u8; 32],
        chunk_refs: Vec<[u8; 32]>,
        payload: Vec<u8>,
        has_inline_data: bool,
    ) -> Result<SequenceNumber, LogError> {
        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes,
            operation,
            hashed_key,
            chunk_refs,
            payload,
            has_inline_data,
        };

        let resp = self
            .raft
            .client_write(cmd)
            .await
            .map_err(|_| LogError::Unavailable)?;

        match resp.response() {
            LogResponse::Appended(seq) => Ok(SequenceNumber(*seq)),
            LogResponse::Ok => Err(LogError::Unavailable),
        }
    }

    /// Set or clear maintenance mode through Raft consensus.
    pub async fn set_maintenance(&self, enabled: bool) -> Result<(), LogError> {
        self.raft
            .client_write(LogCommand::SetMaintenance { enabled })
            .await
            .map_err(|_| LogError::Unavailable)?;

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
    /// Returns a minimal `ShardInfo` based on what the state machine
    /// tracks (`delta_count`, tip, maintenance).
    pub async fn shard_health(&self) -> ShardInfo {
        let inner = self.state.lock().await;
        ShardInfo {
            shard_id: kiseki_common::ids::ShardId(uuid::Uuid::nil()),
            tenant_id: kiseki_common::ids::OrgId(uuid::Uuid::nil()),
            raft_members: vec![],
            leader: None,
            tip: SequenceNumber(inner.tip),
            delta_count: inner.delta_count,
            byte_size: 0,
            state: if inner.maintenance {
                crate::shard::ShardState::Maintenance
            } else {
                crate::shard::ShardState::Healthy
            },
            config: crate::shard::ShardConfig::default(),
            range_start: [0u8; 32],
            range_end: [0xff; 32],
        }
    }
}
