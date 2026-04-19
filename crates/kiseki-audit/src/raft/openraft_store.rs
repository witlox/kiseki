//! OpenRaft-backed audit store.
//!
//! Wraps a `Raft<AuditTypeConfig>` handle for consensus-replicated
//! audit event logging. Reads from shared state machine inner, writes
//! go through `client_write()`.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_raft::{KisekiNode, KisekiRaftConfig, MemLogStore, StubNetworkFactory};
use openraft::Raft;

use super::state_machine::{AuditSmInner, AuditStateMachine};
use super::types::AuditTypeConfig;
use crate::error::AuditError;
use crate::health::{AuditHealth, AuditStatus};
use crate::raft_store::AuditCommand;

type C = AuditTypeConfig;

/// OpenRaft-backed audit store.
///
/// Single-node Raft for now. Writes go through `client_write()`,
/// reads from the shared `AuditSmInner`.
pub struct OpenRaftAuditStore {
    raft: Raft<C, AuditStateMachine>,
    state: Arc<futures::lock::Mutex<AuditSmInner>>,
}

impl OpenRaftAuditStore {
    /// Create and bootstrap a single-node Raft audit store.
    pub async fn new(node_id: u64) -> Result<Self, AuditError> {
        let config = KisekiRaftConfig::default_config();
        let log_store = MemLogStore::<C>::new();
        let state_inner = Arc::new(futures::lock::Mutex::new(AuditSmInner::new()));
        let state_machine = AuditStateMachine::new(Arc::clone(&state_inner));
        let network = StubNetworkFactory::<C>::new();

        let raft = Raft::new(node_id, config, network, log_store, state_machine)
            .await
            .map_err(|_e| AuditError::Unavailable)?;

        // Initialize as single-node cluster.
        let mut members = BTreeMap::new();
        members.insert(node_id, KisekiNode::new("localhost:9103"));
        raft.initialize(members)
            .await
            .map_err(|_| AuditError::Unavailable)?;

        Ok(Self {
            raft,
            state: state_inner,
        })
    }

    /// Append an audit event through Raft consensus.
    pub async fn append_event(
        &self,
        event_type: &str,
        actor: &str,
        tenant_id: Option<[u8; 16]>,
        description: &str,
    ) -> Result<(), AuditError> {
        self.raft
            .client_write(AuditCommand::AppendEvent {
                tenant_id,
                event_type: event_type.to_owned(),
                actor: actor.to_owned(),
                description: description.to_owned(),
            })
            .await
            .map_err(|_| AuditError::Unavailable)?;

        Ok(())
    }

    /// Get the total event count from the state machine.
    pub async fn event_count(&self) -> u64 {
        let inner = self.state.lock().await;
        inner.event_count
    }

    /// Get health status.
    pub async fn health(&self) -> AuditHealth {
        let inner = self.state.lock().await;
        AuditHealth {
            status: AuditStatus::Healthy,
            event_count: inner.event_count,
        }
    }
}
