//! openraft state machine for Log shards.

use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures::TryStreamExt;
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine, Snapshot};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder, StoredMembership};
use serde::{Deserialize, Serialize};

use super::types::{LogResponse, LogTypeConfig};
use crate::raft_store::LogCommand;

type C = LogTypeConfig;

/// Serializable snapshot of a shard's delta state.
#[derive(Clone, Default, Serialize, Deserialize)]
struct ShardSnapshot {
    /// Number of deltas committed.
    delta_count: u64,
    /// Current tip sequence number.
    tip: u64,
    /// Whether in maintenance mode.
    maintenance: bool,
}

/// Inner state for the shard state machine.
pub struct ShardSmInner {
    pub(crate) delta_count: u64,
    pub(crate) tip: u64,
    pub(crate) maintenance: bool,
    last_applied_log: Option<LogIdOf<C>>,
    last_membership: StoredMembershipOf<C>,
}

impl ShardSmInner {
    pub(crate) fn new() -> Self {
        Self {
            delta_count: 0,
            tip: 0,
            maintenance: false,
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
        }
    }

    fn apply_command(&mut self, cmd: &LogCommand) -> LogResponse {
        match cmd {
            LogCommand::AppendDelta { .. } => {
                self.tip += 1;
                self.delta_count += 1;
                LogResponse::Appended(self.tip)
            }
            LogCommand::SetMaintenance { enabled } => {
                self.maintenance = *enabled;
                LogResponse::Ok
            }
            LogCommand::AdvanceWatermark { .. } => LogResponse::Ok,
        }
    }
}

/// openraft state machine for a Log shard.
#[derive(Clone)]
pub struct ShardStateMachine {
    inner: Arc<futures::lock::Mutex<ShardSmInner>>,
}

impl ShardStateMachine {
    pub(crate) fn new(inner: Arc<futures::lock::Mutex<ShardSmInner>>) -> Self {
        Self { inner }
    }
}

impl RaftSnapshotBuilder<C> for ShardStateMachine {
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C>, io::Error> {
        let inner = self.inner.lock().await;
        let snap = ShardSnapshot {
            delta_count: inner.delta_count,
            tip: inner.tip,
            maintenance: inner.maintenance,
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
        Ok(Snapshot {
            meta,
            snapshot: Cursor::new(data),
        })
    }
}

impl RaftStateMachine<C> for ShardStateMachine {
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
                EntryPayload::Blank => LogResponse::Ok,
                EntryPayload::Normal(cmd) => inner.apply_command(cmd),
                EntryPayload::Membership(mem) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), mem.clone());
                    LogResponse::Ok
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
        let snap: ShardSnapshot = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut inner = self.inner.lock().await;
        inner.delta_count = snap.delta_count;
        inner.tip = snap.tip;
        inner.maintenance = snap.maintenance;
        inner.last_applied_log = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        let Some(ref last) = inner.last_applied_log else {
            return Ok(None);
        };
        let snap = ShardSnapshot {
            delta_count: inner.delta_count,
            tip: inner.tip,
            maintenance: inner.maintenance,
        };
        let data = serde_json::to_vec(&snap).map_err(io::Error::other)?;
        let meta = SnapshotMetaOf::<C> {
            last_log_id: Some(*last),
            last_membership: inner.last_membership.clone(),
            snapshot_id: format!("snap-{}", last.index()),
        };
        Ok(Some(Snapshot {
            meta,
            snapshot: Cursor::new(data),
        }))
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }
}
