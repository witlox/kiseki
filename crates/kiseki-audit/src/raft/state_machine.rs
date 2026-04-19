//! openraft state machine for Audit shards — append-only (I-A1).

use std::collections::HashMap;
use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures::TryStreamExt;
use kiseki_common::ids::{OrgId, SequenceNumber};
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine, Snapshot};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder, StoredMembership};
use serde::{Deserialize, Serialize};

use super::types::{AuditResponse, AuditTypeConfig};
use crate::event::{AuditEvent, AuditEventType};
use crate::raft_store::AuditCommand;
use crate::store::AuditQuery;

type C = AuditTypeConfig;

/// Shard key for per-tenant event storage.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub(crate) enum ShardKey {
    Tenant([u8; 16]),
    System,
}

/// Per-shard state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SmShard {
    events: Vec<AuditEvent>,
    tip: SequenceNumber,
}

#[derive(Clone, Serialize, Deserialize)]
struct AuditSnapshot {
    event_count: u64,
    shards: HashMap<ShardKey, SmShard>,
}

pub struct AuditSmInner {
    pub(crate) event_count: u64,
    pub(crate) shards: HashMap<ShardKey, SmShard>,
    last_applied_log: Option<LogIdOf<C>>,
    last_membership: StoredMembershipOf<C>,
}

impl AuditSmInner {
    pub(crate) fn new() -> Self {
        Self {
            event_count: 0,
            shards: HashMap::new(),
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
        }
    }

    /// Query events from the state machine.
    pub(crate) fn query(&self, q: &AuditQuery) -> Vec<AuditEvent> {
        let key = q
            .tenant_id
            .map_or(ShardKey::System, |o| ShardKey::Tenant(*o.0.as_bytes()));
        let Some(shard) = self.shards.get(&key) else {
            return Vec::new();
        };
        shard
            .events
            .iter()
            .filter(|e| e.sequence >= q.from)
            .filter(|e| q.event_type.as_ref().map_or(true, |t| &e.event_type == t))
            .take(q.limit)
            .cloned()
            .collect()
    }

    /// Get tip for a tenant or system shard.
    pub(crate) fn tip(&self, tenant_id: Option<OrgId>) -> SequenceNumber {
        let key = tenant_id.map_or(ShardKey::System, |o| ShardKey::Tenant(*o.0.as_bytes()));
        self.shards.get(&key).map_or(SequenceNumber(0), |s| s.tip)
    }

    /// Total event count across all shards.
    pub(crate) fn total_events(&self) -> usize {
        self.shards.values().map(|s| s.events.len()).sum()
    }

    /// Export all events for a specific tenant.
    pub(crate) fn tenant_export(&self, tenant_id: OrgId) -> Vec<AuditEvent> {
        let key = ShardKey::Tenant(*tenant_id.0.as_bytes());
        let Some(shard) = self.shards.get(&key) else {
            return Vec::new();
        };
        let mut events = shard.events.clone();
        events.sort_by_key(|e| e.sequence);
        events
    }

    /// Apply a command to the state machine, constructing and storing the event.
    fn apply_command(&mut self, cmd: &AuditCommand) {
        let AuditCommand::AppendEvent {
            tenant_id,
            event_type,
            actor,
            description,
        } = cmd;

        let org_id = tenant_id.map(|b| OrgId(uuid::Uuid::from_bytes(b)));
        let key = tenant_id.map_or(ShardKey::System, ShardKey::Tenant);
        let shard = self.shards.entry(key).or_insert_with(|| SmShard {
            events: Vec::new(),
            tip: SequenceNumber(0),
        });

        let next_seq = SequenceNumber(shard.tip.0 + 1);
        let log_index = self
            .last_applied_log
            .as_ref()
            .map_or(0, openraft::LogId::index);
        let timestamp = kiseki_common::time::DeltaTimestamp {
            hlc: kiseki_common::time::HybridLogicalClock {
                physical_ms: log_index,
                logical: 0,
                node_id: kiseki_common::ids::NodeId(0),
            },
            wall: kiseki_common::time::WallTime {
                millis_since_epoch: log_index,
                timezone: "UTC".into(),
            },
            quality: kiseki_common::time::ClockQuality::Ntp,
        };

        shard.events.push(AuditEvent {
            sequence: next_seq,
            timestamp,
            event_type: event_type_from_str(event_type),
            tenant_id: org_id,
            actor: actor.clone(),
            description: description.clone(),
        });
        shard.tip = next_seq;
        self.event_count += 1;
    }
}

fn event_type_from_str(s: &str) -> AuditEventType {
    match s {
        "KeyGeneration" => AuditEventType::KeyGeneration,
        "KeyRotation" => AuditEventType::KeyRotation,
        "KeyDestruction" => AuditEventType::KeyDestruction,
        "KeyAccess" => AuditEventType::KeyAccess,
        "ReEncryption" => AuditEventType::ReEncryption,
        "DataRead" => AuditEventType::DataRead,
        "DataWrite" => AuditEventType::DataWrite,
        "DataDelete" => AuditEventType::DataDelete,
        "AuthSuccess" => AuditEventType::AuthSuccess,
        "AuthFailure" => AuditEventType::AuthFailure,
        "TenantLifecycle" => AuditEventType::TenantLifecycle,
        "PolicyChange" => AuditEventType::PolicyChange,
        "MaintenanceMode" => AuditEventType::MaintenanceMode,
        "AdvisoryWorkflow" => AuditEventType::AdvisoryWorkflow,
        "AdvisoryHint" => AuditEventType::AdvisoryHint,
        "AdvisoryBudgetExceeded" => AuditEventType::AdvisoryBudgetExceeded,
        _ => AuditEventType::AdminAction,
    }
}

#[derive(Clone)]
pub struct AuditStateMachine {
    inner: Arc<futures::lock::Mutex<AuditSmInner>>,
}

impl AuditStateMachine {
    pub(crate) fn new(inner: Arc<futures::lock::Mutex<AuditSmInner>>) -> Self {
        Self { inner }
    }
}

impl RaftSnapshotBuilder<C> for AuditStateMachine {
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<C>, io::Error> {
        let inner = self.inner.lock().await;
        let snap = AuditSnapshot {
            event_count: inner.event_count,
            shards: inner.shards.clone(),
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

impl RaftStateMachine<C> for AuditStateMachine {
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
                EntryPayload::Blank => AuditResponse::Ok,
                EntryPayload::Normal(cmd) => {
                    inner.apply_command(cmd);
                    AuditResponse::Appended
                }
                EntryPayload::Membership(mem) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), mem.clone());
                    AuditResponse::Ok
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
        let snap: AuditSnapshot = serde_json::from_slice(&data)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let mut inner = self.inner.lock().await;
        inner.event_count = snap.event_count;
        inner.shards = snap.shards;
        inner.last_applied_log = meta.last_log_id;
        inner.last_membership = meta.last_membership.clone();
        Ok(())
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<C>>, io::Error> {
        let inner = self.inner.lock().await;
        let Some(ref last) = inner.last_applied_log else {
            return Ok(None);
        };
        let snap = AuditSnapshot {
            event_count: inner.event_count,
            shards: inner.shards.clone(),
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
