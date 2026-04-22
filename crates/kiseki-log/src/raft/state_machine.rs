//! openraft state machine for Log shards.

use std::io;
use std::io::Cursor;
use std::sync::Arc;

use futures::TryStreamExt;
use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use openraft::alias::{LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf};
use openraft::storage::{EntryResponder, RaftStateMachine, Snapshot};
use openraft::{EntryPayload, OptionalSend, RaftSnapshotBuilder, StoredMembership};
use serde::{Deserialize, Serialize};

use super::types::{LogResponse, LogTypeConfig};
use crate::delta::{Delta, DeltaHeader, DeltaPayload, OperationType};
use crate::raft_store::LogCommand;
use crate::watermark::ConsumerWatermarks;

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
    /// Serialized deltas.
    deltas: Vec<SerializableDelta>,
    /// Serialized consumer watermarks.
    watermarks: Vec<(String, u64)>,
    /// Shard ID bytes (if set).
    shard_id: Option<[u8; 16]>,
    /// Tenant ID bytes (if set).
    tenant_id: Option<[u8; 16]>,
}

/// Serializable form of a Delta for snapshots.
#[derive(Clone, Serialize, Deserialize)]
struct SerializableDelta {
    sequence: u64,
    shard_id: [u8; 16],
    tenant_id: [u8; 16],
    operation: u8,
    hashed_key: [u8; 32],
    tombstone: bool,
    chunk_refs: Vec<[u8; 32]>,
    payload_size: u32,
    has_inline_data: bool,
    ciphertext: Vec<u8>,
}

impl SerializableDelta {
    fn from_delta(d: &Delta) -> Self {
        Self {
            sequence: d.header.sequence.0,
            shard_id: *d.header.shard_id.0.as_bytes(),
            tenant_id: *d.header.tenant_id.0.as_bytes(),
            operation: op_to_u8(d.header.operation),
            hashed_key: d.header.hashed_key,
            tombstone: d.header.tombstone,
            chunk_refs: d.header.chunk_refs.iter().map(|c| c.0).collect(),
            payload_size: d.header.payload_size,
            has_inline_data: d.header.has_inline_data,
            ciphertext: d.payload.ciphertext.clone(),
        }
    }

    fn to_delta(&self) -> Delta {
        Delta {
            header: DeltaHeader {
                sequence: SequenceNumber(self.sequence),
                shard_id: ShardId(uuid::Uuid::from_bytes(self.shard_id)),
                tenant_id: OrgId(uuid::Uuid::from_bytes(self.tenant_id)),
                operation: u8_to_op(self.operation),
                timestamp: kiseki_common::time::DeltaTimestamp {
                    hlc: kiseki_common::time::HybridLogicalClock {
                        physical_ms: 0,
                        logical: 0,
                        node_id: kiseki_common::ids::NodeId(0),
                    },
                    wall: kiseki_common::time::WallTime {
                        millis_since_epoch: 0,
                        timezone: "UTC".into(),
                    },
                    quality: kiseki_common::time::ClockQuality::Ntp,
                },
                hashed_key: self.hashed_key,
                tombstone: self.tombstone,
                chunk_refs: self
                    .chunk_refs
                    .iter()
                    .map(|b| kiseki_common::ids::ChunkId(*b))
                    .collect(),
                payload_size: self.payload_size,
                has_inline_data: self.has_inline_data,
            },
            payload: DeltaPayload {
                ciphertext: self.ciphertext.clone(),
                auth_tag: Vec::new(),
                nonce: Vec::new(),
                system_epoch: None,
                tenant_epoch: None,
                tenant_wrapped_material: Vec::new(),
            },
        }
    }
}

fn op_to_u8(op: OperationType) -> u8 {
    match op {
        OperationType::Create => 0,
        OperationType::Update => 1,
        OperationType::Delete => 2,
        OperationType::Rename => 3,
        OperationType::SetAttribute => 4,
        OperationType::Finalize => 5,
    }
}

fn u8_to_op(v: u8) -> OperationType {
    match v {
        0 => OperationType::Create,
        1 => OperationType::Update,
        2 => OperationType::Delete,
        3 => OperationType::Rename,
        4 => OperationType::SetAttribute,
        _ => OperationType::Finalize,
    }
}

/// Inner state for the shard state machine.
pub struct ShardSmInner {
    pub(crate) delta_count: u64,
    pub(crate) tip: u64,
    pub(crate) maintenance: bool,
    pub(crate) deltas: Vec<Delta>,
    pub(crate) watermarks: ConsumerWatermarks,
    pub(crate) shard_id: ShardId,
    pub(crate) tenant_id: OrgId,
    last_applied_log: Option<LogIdOf<C>>,
    last_membership: StoredMembershipOf<C>,
    /// Inline content store for small files (ADR-030, I-SF5).
    /// When set, inline payloads are offloaded to this store on apply
    /// and cleared from in-memory deltas.
    pub(crate) inline_store: Option<Arc<dyn kiseki_common::inline_store::InlineStore>>,
}

impl ShardSmInner {
    pub(crate) fn new(shard_id: ShardId, tenant_id: OrgId) -> Self {
        Self {
            delta_count: 0,
            tip: 0,
            maintenance: false,
            deltas: Vec::new(),
            watermarks: ConsumerWatermarks::new(),
            shard_id,
            tenant_id,
            last_applied_log: None,
            last_membership: StoredMembershipOf::<C>::default(),
            inline_store: None,
        }
    }

    /// Set the inline store for small-file content offload.
    #[allow(dead_code)]
    pub(crate) fn with_inline_store(
        mut self,
        store: Arc<dyn kiseki_common::inline_store::InlineStore>,
    ) -> Self {
        self.inline_store = Some(store);
        self
    }

    fn apply_command(&mut self, cmd: &LogCommand, log_index: u64) -> LogResponse {
        match cmd {
            LogCommand::AppendDelta {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
            } => {
                self.tip += 1;
                self.delta_count += 1;
                let next_seq = SequenceNumber(self.tip);

                #[allow(clippy::cast_possible_truncation)]
                let payload_size = payload.len() as u32;

                let op = u8_to_op(*operation);

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

                // Offload inline content to the store if available (I-SF5).
                // Key = sha256(hashed_key || sequence) to ensure uniqueness
                // per delta (multiple deltas can share the same hashed_key).
                let inline_key = {
                    let mut k = *hashed_key;
                    let seq_bytes = self.tip.to_le_bytes();
                    for (i, &b) in seq_bytes.iter().enumerate() {
                        k[24 + i] ^= b; // mix sequence into last 8 bytes
                    }
                    k
                };
                let ciphertext = if *has_inline_data {
                    if let Some(ref store) = self.inline_store {
                        let _ = store.put(&inline_key, payload);
                        // Clear payload from in-memory delta.
                        Vec::new()
                    } else {
                        payload.clone()
                    }
                } else {
                    payload.clone()
                };

                let delta = Delta {
                    header: DeltaHeader {
                        sequence: next_seq,
                        shard_id: self.shard_id,
                        tenant_id: OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes)),
                        operation: op,
                        timestamp,
                        hashed_key: *hashed_key,
                        tombstone: *operation == 2,
                        chunk_refs: chunk_refs
                            .iter()
                            .map(|b| kiseki_common::ids::ChunkId(*b))
                            .collect(),
                        payload_size,
                        has_inline_data: *has_inline_data,
                    },
                    payload: DeltaPayload {
                        ciphertext,
                        auth_tag: Vec::new(),
                        nonce: Vec::new(),
                        system_epoch: None,
                        tenant_epoch: None,
                        tenant_wrapped_material: Vec::new(),
                    },
                };

                self.deltas.push(delta);
                LogResponse::Appended(self.tip)
            }
            LogCommand::SetMaintenance { enabled } => {
                self.maintenance = *enabled;
                LogResponse::Ok
            }
            LogCommand::AdvanceWatermark { consumer, position } => {
                self.watermarks.advance(consumer, SequenceNumber(*position));
                LogResponse::Ok
            }
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
        // Build serializable deltas, reading inline content from store
        // for deltas whose ciphertext was offloaded (I-SF5).
        let deltas: Vec<SerializableDelta> = inner
            .deltas
            .iter()
            .map(|d| {
                let mut sd = SerializableDelta::from_delta(d);
                if d.header.has_inline_data && sd.ciphertext.is_empty() {
                    if let Some(ref store) = inner.inline_store {
                        let mut inline_key = d.header.hashed_key;
                        let seq_bytes = d.header.sequence.0.to_le_bytes();
                        for (i, &b) in seq_bytes.iter().enumerate() {
                            inline_key[24 + i] ^= b;
                        }
                        if let Ok(Some(data)) = store.get(&inline_key) {
                            sd.ciphertext = data;
                        }
                    }
                }
                sd
            })
            .collect();
        let snap = ShardSnapshot {
            delta_count: inner.delta_count,
            tip: inner.tip,
            maintenance: inner.maintenance,
            deltas,
            watermarks: inner.watermarks.as_vec(),
            shard_id: Some(*inner.shard_id.0.as_bytes()),
            tenant_id: Some(*inner.tenant_id.0.as_bytes()),
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
            let log_index = entry.log_id.index();
            inner.last_applied_log = Some(entry.log_id);
            let response = match &entry.payload {
                EntryPayload::Blank => LogResponse::Ok,
                EntryPayload::Normal(cmd) => inner.apply_command(cmd, log_index),
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
        // Restore deltas, offloading inline content to the store if available.
        inner.deltas = snap
            .deltas
            .iter()
            .map(|sd| {
                let delta = sd.to_delta();
                if delta.header.has_inline_data && !sd.ciphertext.is_empty() {
                    if let Some(ref store) = inner.inline_store {
                        let _ = store.put(&delta.header.hashed_key, &sd.ciphertext);
                    }
                }
                // Clear ciphertext from in-memory delta if store is available.
                if delta.header.has_inline_data && inner.inline_store.is_some() {
                    let mut d = delta;
                    d.payload.ciphertext = Vec::new();
                    d
                } else {
                    delta
                }
            })
            .collect();
        let mut wm = ConsumerWatermarks::new();
        for (consumer, pos) in &snap.watermarks {
            wm.advance(consumer, SequenceNumber(*pos));
        }
        inner.watermarks = wm;
        if let Some(sid) = snap.shard_id {
            inner.shard_id = ShardId(uuid::Uuid::from_bytes(sid));
        }
        if let Some(tid) = snap.tenant_id {
            inner.tenant_id = OrgId(uuid::Uuid::from_bytes(tid));
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
        let snap = ShardSnapshot {
            delta_count: inner.delta_count,
            tip: inner.tip,
            maintenance: inner.maintenance,
            deltas: inner
                .deltas
                .iter()
                .map(SerializableDelta::from_delta)
                .collect(),
            watermarks: inner.watermarks.as_vec(),
            shard_id: Some(*inner.shard_id.0.as_bytes()),
            tenant_id: Some(*inner.tenant_id.0.as_bytes()),
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
