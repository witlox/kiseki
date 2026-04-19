//! openraft state machine for the key manager.

use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures::TryStreamExt;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::keys::SystemMasterKey;
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder};
use serde::{Deserialize, Serialize};

use super::types::{KeyResponse, KeyTypeConfig};
use crate::raft_store::KeyCommand;

type C = KeyTypeConfig;

#[derive(Clone, Serialize, Deserialize)]
struct SnapshotEpoch {
    epoch: u64,
    key_material: Vec<u8>,
    is_current: bool,
    migration_complete: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct KeySnapshot {
    epochs: Vec<SnapshotEpoch>,
}

/// An epoch entry in the live state machine.
pub(crate) struct EpochEntry {
    pub key: Arc<SystemMasterKey>,
    pub is_current: bool,
    pub migration_complete: bool,
}

/// Inner state shared with `RaftKeyStore` for reads.
pub(crate) struct StateMachineInner {
    pub epochs: Vec<EpochEntry>,
    pub last_applied_log: Option<LogIdOf<C>>,
    pub last_membership: StoredMembershipOf<C>,
}

impl StateMachineInner {
    pub(crate) fn new() -> Self {
        Self {
            epochs: Vec::new(),
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
        }
    }

    fn apply_command(&mut self, cmd: &KeyCommand) -> KeyResponse {
        match cmd {
            KeyCommand::CreateEpoch {
                epoch,
                key_material,
            } => {
                if self.epochs.iter().any(|e| e.key.epoch == KeyEpoch(*epoch)) {
                    return KeyResponse::Epoch(*epoch);
                }
                let mut material = [0u8; 32];
                let len = key_material.len().min(32);
                material[..len].copy_from_slice(&key_material[..len]);
                for entry in &mut self.epochs {
                    entry.is_current = false;
                }
                self.epochs.push(EpochEntry {
                    key: Arc::new(SystemMasterKey::new(material, KeyEpoch(*epoch))),
                    is_current: true,
                    migration_complete: false,
                });
                KeyResponse::Epoch(*epoch)
            }
            KeyCommand::RotateToEpoch { epoch } => {
                for entry in &mut self.epochs {
                    entry.is_current = entry.key.epoch == KeyEpoch(*epoch);
                }
                KeyResponse::Epoch(*epoch)
            }
            KeyCommand::MarkMigrationComplete { epoch } => {
                if let Some(entry) = self
                    .epochs
                    .iter_mut()
                    .find(|e| e.key.epoch == KeyEpoch(*epoch))
                {
                    entry.migration_complete = true;
                }
                KeyResponse::Ok
            }
        }
    }
}

#[derive(Clone)]
pub struct KeyStateMachine {
    pub(crate) inner: Arc<futures::lock::Mutex<StateMachineInner>>,
}

impl KeyStateMachine {
    pub(crate) fn new(inner: Arc<futures::lock::Mutex<StateMachineInner>>) -> Self {
        Self { inner }
    }
}

impl RaftSnapshotBuilder<C> for KeyStateMachine {
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C>, io::Error> {
        let inner = self.inner.lock().await;
        let snap = KeySnapshot {
            epochs: inner
                .epochs
                .iter()
                .map(|e| SnapshotEpoch {
                    epoch: e.key.epoch.0,
                    key_material: e.key.material().to_vec(),
                    is_current: e.is_current,
                    migration_complete: e.migration_complete,
                })
                .collect(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let snapshot_id = format!(
            "snap-{}",
            inner
                .last_applied_log
                .as_ref()
                .map_or(0, openraft::LogId::index)
        );
        let meta = SnapshotMetaOf::<C> {
            last_log_id: inner.last_applied_log,
            last_membership: inner.last_membership.clone(),
            snapshot_id,
        };
        Ok(openraft::storage::Snapshot {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl RaftStateMachine<C> for KeyStateMachine {
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf<C>>, StoredMembershipOf<C>), io::Error> {
        let inner = self.inner.lock().await;
        Ok((inner.last_applied_log, inner.last_membership.clone()))
    }

    async fn apply<Strm>(&mut self, mut entries: Strm) -> Result<(), io::Error>
    where
        Strm: futures::Stream<Item = Result<EntryResponder<C>, io::Error>> + Unpin + OptionalSend,
    {
        let mut inner = self.inner.lock().await;
        while let Some((entry, responder)) = entries.try_next().await? {
            inner.last_applied_log = Some(entry.log_id);
            let response = match &entry.payload {
                EntryPayload::Blank => KeyResponse::Ok,
                EntryPayload::Normal(cmd) => inner.apply_command(cmd),
                EntryPayload::Membership(mem) => {
                    inner.last_membership =
                        openraft::StoredMembership::new(Some(entry.log_id), mem.clone());
                    KeyResponse::Ok
                }
            };
            if let Some(responder) = responder {
                responder.send(response);
            }
        }
        Ok(())
    }

    async fn begin_receiving_snapshot(
        &mut self,
    ) -> Result<<C as openraft::RaftTypeConfig>::SnapshotData, io::Error> {
        Ok(Cursor::new(Vec::new()))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<C>,
        snapshot: <C as openraft::RaftTypeConfig>::SnapshotData,
    ) -> Result<(), io::Error> {
        let data = snapshot.into_inner();
        let snap: KeySnapshot = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut inner = self.inner.lock().await;
        inner.epochs.clear();
        for se in &snap.epochs {
            let mut material = [0u8; 32];
            let len = se.key_material.len().min(32);
            material[..len].copy_from_slice(&se.key_material[..len]);
            inner.epochs.push(EpochEntry {
                key: Arc::new(SystemMasterKey::new(material, KeyEpoch(se.epoch))),
                is_current: se.is_current,
                migration_complete: se.migration_complete,
            });
        }
        inner.last_applied_log = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        let Some(ref last) = inner.last_applied_log else {
            return Ok(None);
        };
        let snap = KeySnapshot {
            epochs: inner
                .epochs
                .iter()
                .map(|e| SnapshotEpoch {
                    epoch: e.key.epoch.0,
                    key_material: e.key.material().to_vec(),
                    is_current: e.is_current,
                    migration_complete: e.migration_complete,
                })
                .collect(),
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let meta = SnapshotMetaOf::<C> {
            last_log_id: Some(*last),
            last_membership: inner.last_membership.clone(),
            snapshot_id: format!("snap-{}", last.index()),
        };
        Ok(Some(openraft::storage::Snapshot {
            meta,
            snapshot: Cursor::new(data),
        }))
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}
