//! OpenRaft-backed audit store.
//!
//! Wraps a `Raft<AuditTypeConfig>` handle for consensus-replicated
//! audit event logging. Reads from shared state machine inner, writes
//! go through `client_write()`.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_raft::{KisekiNode, KisekiRaftConfig, MemLogStore, StubNetworkFactory};
use openraft::Raft;

use kiseki_common::ids::{OrgId, SequenceNumber};

use super::state_machine::{AuditSmInner, AuditStateMachine};
use super::types::AuditTypeConfig;
use crate::error::AuditError;
use crate::event::AuditEvent;
use crate::health::{AuditHealth, AuditStatus};
use crate::raft_store::AuditCommand;
use crate::store::AuditQuery;

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

    /// Append an `AuditEvent` through Raft consensus.
    ///
    /// This is the async equivalent of `AuditOps::append`. The event is
    /// serialized into an `AuditCommand::AppendEvent` and written through
    /// the Raft log for consensus-replicated, append-only storage.
    pub async fn append(&self, event: AuditEvent) -> Result<(), AuditError> {
        self.raft
            .client_write(AuditCommand::AppendEvent {
                tenant_id: event.tenant_id.map(|o| *o.0.as_bytes()),
                event_type: crate::raft_store::RaftAuditStore::event_type_to_str_pub(
                    &event.event_type,
                )
                .to_owned(),
                actor: event.actor,
                description: event.description,
            })
            .await
            .map_err(|_| AuditError::Unavailable)?;
        Ok(())
    }

    /// Query audit events from the shared state machine.
    ///
    /// Async equivalent of `AuditOps::query`.
    pub async fn query(&self, q: &AuditQuery) -> Vec<AuditEvent> {
        let inner = self.state.lock().await;
        inner.query(q)
    }

    /// Get the tip (latest sequence number) for a tenant or system shard.
    ///
    /// Async equivalent of `AuditOps::tip`.
    pub async fn tip(&self, tenant_id: Option<OrgId>) -> SequenceNumber {
        let inner = self.state.lock().await;
        inner.tip(tenant_id)
    }

    /// Total event count across all shards.
    ///
    /// Async equivalent of `AuditOps::total_events`.
    pub async fn total_events(&self) -> usize {
        let inner = self.state.lock().await;
        inner.total_events()
    }

    /// Export all events for a specific tenant.
    ///
    /// Async equivalent of `AuditOps::tenant_export`.
    pub async fn tenant_export(&self, tenant_id: OrgId) -> Vec<AuditEvent> {
        let inner = self.state.lock().await;
        inner.tenant_export(tenant_id)
    }
}
