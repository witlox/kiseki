//! OpenRaft-backed key store.
//!
//! Wraps a `Raft<KeyTypeConfig>` handle for consensus-replicated
//! key management. Reads from shared state machine inner, writes
//! go through `client_write()`.

use std::collections::BTreeMap;
use std::sync::Arc;

use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_raft::{
    tcp_transport, KisekiNode, KisekiRaftConfig, MemLogStore, StubNetworkFactory, TcpNetworkFactory,
};
use openraft::Raft;

use super::state_machine::{KeyStateMachine, StateMachineInner};
use super::types::KeyTypeConfig;
use crate::epoch::{EpochInfo, KeyManagerOps};
use crate::error::KeyManagerError;
use crate::health::{KeyManagerHealth, KeyManagerStatus};
use crate::raft_store::KeyCommand;

type C = KeyTypeConfig;

/// OpenRaft-backed key store.
///
/// Single-node Raft for now. Writes go through `client_write()`,
/// reads from the shared `StateMachineInner`.
pub struct OpenRaftKeyStore {
    raft: Raft<C, KeyStateMachine>,
    state: Arc<futures::lock::Mutex<StateMachineInner>>,
}

impl OpenRaftKeyStore {
    /// Create and bootstrap a Raft key store.
    ///
    /// When `peers` is empty, runs single-node with stub network.
    /// When `peers` has entries, uses TCP transport for multi-node Raft.
    pub async fn new(node_id: u64, peers: &BTreeMap<u64, String>) -> Result<Self, KeyManagerError> {
        let config = KisekiRaftConfig::default_config();
        let log_store = MemLogStore::<C>::new();
        let state_inner = Arc::new(futures::lock::Mutex::new(StateMachineInner::new()));
        let state_machine = KeyStateMachine::new(Arc::clone(&state_inner));

        let members: BTreeMap<u64, KisekiNode> = if peers.len() > 1 {
            peers
                .iter()
                .map(|(id, addr)| (*id, KisekiNode::new(addr)))
                .collect()
        } else {
            let mut m = BTreeMap::new();
            let addr = peers.get(&node_id).map_or("localhost:9102", String::as_str);
            m.insert(node_id, KisekiNode::new(addr));
            m
        };

        let raft = if peers.len() > 1 {
            let network = TcpNetworkFactory::<C>::new();
            Raft::new(node_id, config, network, log_store, state_machine)
                .await
                .map_err(|_e| KeyManagerError::Unavailable)?
        } else {
            let network = StubNetworkFactory::<C>::new();
            Raft::new(node_id, config, network, log_store, state_machine)
                .await
                .map_err(|_e| KeyManagerError::Unavailable)?
        };

        raft.initialize(members)
            .await
            .map_err(|_| KeyManagerError::Unavailable)?;

        // Bootstrap: create initial epoch via Raft consensus.
        let mut key_material = [0u8; 32];
        aws_lc_rs::rand::fill(&mut key_material)
            .map_err(|_| KeyManagerError::KeyGenerationFailed)?;

        raft.client_write(KeyCommand::CreateEpoch {
            epoch: 1,
            key_material: key_material.to_vec(),
        })
        .await
        .map_err(|_| KeyManagerError::Unavailable)?;

        raft.client_write(KeyCommand::MarkMigrationComplete { epoch: 1 })
            .await
            .map_err(|_| KeyManagerError::Unavailable)?;

        Ok(Self {
            raft,
            state: state_inner,
        })
    }

    /// Spawn the Raft RPC server for the key manager Raft group.
    #[must_use]
    pub fn spawn_rpc_server(
        &self,
        addr: String,
    ) -> tokio::task::JoinHandle<Result<(), std::io::Error>> {
        let raft = Arc::new(self.raft.clone());
        tokio::spawn(async move { tcp_transport::run_raft_rpc_server::<C>(&addr, raft).await })
    }

    /// Get health status.
    pub async fn health(&self) -> KeyManagerHealth {
        let inner = self.state.lock().await;
        let current_epoch = inner
            .epochs
            .iter()
            .find(|e| e.is_current)
            .map(|e| e.key.epoch.0);
        KeyManagerHealth {
            status: KeyManagerStatus::Healthy,
            epoch_count: inner.epochs.len(),
            current_epoch,
        }
    }
}

#[tonic::async_trait]
impl KeyManagerOps for OpenRaftKeyStore {
    async fn fetch_master_key(
        &self,
        epoch: KeyEpoch,
    ) -> Result<Arc<SystemMasterKey>, KeyManagerError> {
        let inner = self.state.lock().await;
        inner
            .epochs
            .iter()
            .find(|e| e.key.epoch == epoch)
            .map(|e| Arc::clone(&e.key))
            .ok_or(KeyManagerError::EpochNotFound(epoch))
    }

    async fn current_epoch(&self) -> Result<KeyEpoch, KeyManagerError> {
        let inner = self.state.lock().await;
        inner
            .epochs
            .iter()
            .find(|e| e.is_current)
            .map(|e| e.key.epoch)
            .ok_or(KeyManagerError::Unavailable)
    }

    async fn rotate(&self) -> Result<KeyEpoch, KeyManagerError> {
        let next_epoch = {
            let inner = self.state.lock().await;
            inner
                .epochs
                .iter()
                .map(|e| e.key.epoch.0)
                .max()
                .unwrap_or(0)
                + 1
        };

        let mut key_material = [0u8; 32];
        aws_lc_rs::rand::fill(&mut key_material)
            .map_err(|_| KeyManagerError::KeyGenerationFailed)?;

        self.raft
            .client_write(KeyCommand::CreateEpoch {
                epoch: next_epoch,
                key_material: key_material.to_vec(),
            })
            .await
            .map_err(|_| KeyManagerError::Unavailable)?;

        Ok(KeyEpoch(next_epoch))
    }

    async fn mark_migration_complete(&self, epoch: KeyEpoch) -> Result<(), KeyManagerError> {
        {
            let inner = self.state.lock().await;
            if !inner.epochs.iter().any(|e| e.key.epoch == epoch) {
                return Err(KeyManagerError::EpochNotFound(epoch));
            }
        }

        self.raft
            .client_write(KeyCommand::MarkMigrationComplete { epoch: epoch.0 })
            .await
            .map_err(|_| KeyManagerError::Unavailable)?;

        Ok(())
    }

    async fn list_epochs(&self) -> Vec<EpochInfo> {
        let inner = self.state.lock().await;
        inner
            .epochs
            .iter()
            .map(|e| EpochInfo {
                epoch: e.key.epoch,
                is_current: e.is_current,
                migration_complete: e.migration_complete,
            })
            .collect()
    }
}
