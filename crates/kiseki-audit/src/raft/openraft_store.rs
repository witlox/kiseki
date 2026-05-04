//! OpenRaft-backed audit store.
//!
//! Wraps a `Raft<AuditTypeConfig>` handle for consensus-replicated
//! audit event logging. Reads from shared state machine inner, writes
//! go through `client_write()`.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_raft::{
    tcp_transport, KisekiNode, KisekiRaftConfig, MemLogStore, RedbRaftLogStore, StubNetworkFactory,
    TcpNetworkFactory,
};
use openraft::Raft;

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use uuid::Uuid;

/// Constant `ShardId` for the audit-log Raft group. Per-tenant
/// audit shards are still distinct logical groups (ADR-009); the
/// `ShardId` here represents the multiplexed-transport routing key
/// for ALL audit Raft traffic on a single node, distinct from the
/// log shards. Pick a deterministic UUID derived from `audit_RG`.
pub const AUDIT_RAFT_GROUP_ID: ShardId = ShardId(Uuid::from_u128(
    0x6175_6469_745f_5261_6674_4772_6f75_7000_u128, // "audit_RaftGroup" ASCII
));

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
    /// Create and bootstrap a Raft audit store.
    ///
    /// When `peers` is empty, runs single-node with stub network.
    /// When `peers` has entries, uses TCP transport for multi-node Raft.
    pub async fn new(
        node_id: u64,
        peers: &BTreeMap<u64, String>,
        data_dir: Option<&std::path::Path>,
    ) -> Result<Self, AuditError> {
        let config = KisekiRaftConfig::default_config();
        let state_inner = Arc::new(futures::lock::Mutex::new(AuditSmInner::new()));
        let state_machine = AuditStateMachine::new(Arc::clone(&state_inner));

        let members: BTreeMap<u64, KisekiNode> = if peers.len() > 1 {
            peers
                .iter()
                .map(|(id, addr)| (*id, KisekiNode::new(addr)))
                .collect()
        } else {
            let mut m = BTreeMap::new();
            let addr = peers.get(&node_id).map_or("localhost:9103", String::as_str);
            m.insert(node_id, KisekiNode::new(addr));
            m
        };

        let (raft, already_initialized) = if let Some(dir) = data_dir {
            let raft_dir = dir.join("raft");
            std::fs::create_dir_all(&raft_dir).ok();
            let redb_path = raft_dir.join("audit.redb");
            let log_store =
                RedbRaftLogStore::<C>::open(&redb_path).map_err(|_| AuditError::Unavailable)?;
            let has_state = log_store.has_state();
            let raft = if peers.len() > 1 {
                let network = TcpNetworkFactory::<C>::new(AUDIT_RAFT_GROUP_ID);
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| AuditError::Unavailable)?
            } else {
                let network = StubNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| AuditError::Unavailable)?
            };
            (raft, has_state)
        } else {
            let log_store = MemLogStore::<C>::new();
            let raft = if peers.len() > 1 {
                let network = TcpNetworkFactory::<C>::new(AUDIT_RAFT_GROUP_ID);
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| AuditError::Unavailable)?
            } else {
                let network = StubNetworkFactory::<C>::new();
                Raft::new(node_id, config, network, log_store, state_machine)
                    .await
                    .map_err(|_e| AuditError::Unavailable)?
            };
            (raft, false)
        };

        if !already_initialized {
            raft.initialize(members)
                .await
                .map_err(|_| AuditError::Unavailable)?;
        }

        Ok(Self {
            raft,
            state: state_inner,
        })
    }

    /// Spawn the Raft RPC server for the audit Raft group.
    /// Uses the multiplexed transport (ADR-041) with a single
    /// registered shard at `AUDIT_RAFT_GROUP_ID`.
    #[must_use]
    pub fn spawn_rpc_server(
        &self,
        addr: String,
    ) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
        let raft = Arc::new(self.raft.clone());
        tokio::spawn(async move {
            tcp_transport::run_single_raft_group_listener::<C>(
                &addr,
                AUDIT_RAFT_GROUP_ID,
                raft,
                None,
            )
            .await
        })
    }

    /// Append an audit event through Raft consensus.
    #[tracing::instrument(skip(self, description), fields(event_type, actor, has_tenant = tenant_id.is_some()))]
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
            .map_err(|e| {
                tracing::warn!(error = ?e, "audit append_event: Raft client_write failed");
                AuditError::Unavailable
            })?;

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
