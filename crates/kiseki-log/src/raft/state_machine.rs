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
use crate::raft_store::{LogCommand, NewChunkMeta};
use crate::watermark::ConsumerWatermarks;
use kiseki_common::ids::ChunkId;
use std::collections::HashMap;

/// `cluster_chunk_state` row — Raft-replicated chunk metadata
/// (Phase 16a, D-4). Keyed by `(tenant_id, chunk_id)` so cross-
/// tenant dedup doesn't leak refcounts (I-T1; round-2 fix).
///
/// Distinct from the local `chunk_meta` redb table in ADR-022:
/// that one maps `chunk_id → (device_id, offset, size, fragment_idx)`
/// for the on-disk layout. This one is cluster-wide replication
/// metadata.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ClusterChunkStateEntry {
    /// Number of compositions referencing this chunk in this tenant.
    pub refcount: u64,
    /// Node IDs holding fragments for this chunk. Replication-N has
    /// N entries; EC X+Y has X+Y entries.
    pub placement: Vec<u64>,
    /// True when refcount reached 0 — the entry is held until the
    /// next compaction prunes it (preserves audit trail across
    /// concurrent reads in flight when the decrement applied).
    pub tombstoned: bool,
    /// Apply-time millisecond stamp from the Raft log index. Used
    /// by the orphan-fragment scrub to compute the 24h TTL
    /// (Risk #5 in the implementation plan).
    pub created_ms: u64,
    /// Phase 16d step 3: pre-encode ciphertext length. Lets the
    /// read path size the decoded output exactly under EC mode
    /// instead of relying on a trim-trailing-zeros heuristic.
    /// Defaults to 0 for entries created before 16d (round-trip
    /// safe via serde's default).
    #[serde(default)]
    pub original_len: u64,
}

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
        OperationType::NamespaceCreate => 6,
    }
}

fn u8_to_op(v: u8) -> OperationType {
    match v {
        0 => OperationType::Create,
        1 => OperationType::Update,
        2 => OperationType::Delete,
        3 => OperationType::Rename,
        4 => OperationType::SetAttribute,
        5 => OperationType::Finalize,
        _ => OperationType::NamespaceCreate,
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
    /// `cluster_chunk_state` table (Phase 16a, D-4).
    /// Raft-replicated chunk metadata keyed by `(tenant, chunk_id)`.
    /// See `ClusterChunkStateEntry` doc for the contract.
    pub cluster_chunk_state: HashMap<(OrgId, ChunkId), ClusterChunkStateEntry>,
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
            cluster_chunk_state: HashMap::new(),
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

    #[allow(clippy::too_many_arguments)] // mirrors delta-args structure on the wire
    fn append_delta_inner(
        &mut self,
        tenant_id_bytes: &[u8; 16],
        operation: u8,
        hashed_key: &[u8; 32],
        chunk_refs: &[[u8; 32]],
        payload: &[u8],
        has_inline_data: bool,
        log_index: u64,
    ) -> u64 {
        self.tip += 1;
        self.delta_count += 1;
        let next_seq = SequenceNumber(self.tip);

        #[allow(clippy::cast_possible_truncation)]
        let payload_size = payload.len() as u32;

        let op = u8_to_op(operation);

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
        // Key = hashed_key XOR sequence (last 8 bytes), so two deltas
        // with the same hashed_key but different sequences produce
        // different inline keys.
        let inline_key = {
            let mut k = *hashed_key;
            let seq_bytes = self.tip.to_le_bytes();
            for (i, &b) in seq_bytes.iter().enumerate() {
                k[24 + i] ^= b;
            }
            k
        };
        let ciphertext = if has_inline_data {
            if let Some(ref store) = self.inline_store {
                let _ = store.put(&inline_key, payload);
                Vec::new()
            } else {
                payload.to_vec()
            }
        } else {
            payload.to_vec()
        };

        let delta = Delta {
            header: DeltaHeader {
                sequence: next_seq,
                shard_id: self.shard_id,
                tenant_id: OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes)),
                operation: op,
                timestamp,
                hashed_key: *hashed_key,
                tombstone: operation == 2,
                chunk_refs: chunk_refs.iter().map(|b| ChunkId(*b)).collect(),
                payload_size,
                has_inline_data,
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
        self.tip
    }

    /// Apply Phase 16a `cluster_chunk_state` mutations: create new
    /// entries for each `NewChunkMeta`. Idempotent on re-apply
    /// (existing key keeps its current refcount + placement).
    fn apply_new_chunks(
        &mut self,
        tenant_id_bytes: &[u8; 16],
        new_chunks: &[NewChunkMeta],
        log_index: u64,
    ) {
        let tenant = OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes));
        for nc in new_chunks {
            let key = (tenant, ChunkId(nc.chunk_id));
            self.cluster_chunk_state
                .entry(key)
                .or_insert_with(|| ClusterChunkStateEntry {
                    refcount: 1,
                    placement: nc.placement.clone(),
                    tombstoned: false,
                    created_ms: log_index,
                    original_len: nc.original_len,
                });
        }
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
                let tip = self.append_delta_inner(
                    tenant_id_bytes,
                    *operation,
                    hashed_key,
                    chunk_refs,
                    payload,
                    *has_inline_data,
                    log_index,
                );
                LogResponse::Appended(tip)
            }
            LogCommand::ChunkAndDelta {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
                new_chunks,
            } => {
                // Atomic per D-4 round 2: chunk_meta entries are
                // created BEFORE the delta is appended so a reader
                // observing the delta after this apply step always
                // finds the corresponding cluster_chunk_state.
                self.apply_new_chunks(tenant_id_bytes, new_chunks, log_index);
                let tip = self.append_delta_inner(
                    tenant_id_bytes,
                    *operation,
                    hashed_key,
                    chunk_refs,
                    payload,
                    *has_inline_data,
                    log_index,
                );
                LogResponse::Appended(tip)
            }
            LogCommand::IncrementChunkRefcount {
                tenant_id_bytes,
                chunk_id,
            } => {
                let tenant = OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes));
                let key = (tenant, ChunkId(*chunk_id));
                if let Some(entry) = self.cluster_chunk_state.get_mut(&key) {
                    entry.refcount = entry.refcount.saturating_add(1);
                    // A new reference revives a tombstoned entry —
                    // unusual (would mean concurrent decrement +
                    // re-create) but defensible.
                    entry.tombstoned = false;
                }
                LogResponse::Ok
            }
            LogCommand::DecrementChunkRefcount {
                tenant_id_bytes,
                chunk_id,
            } => {
                let tenant = OrgId(uuid::Uuid::from_bytes(*tenant_id_bytes));
                let key = (tenant, ChunkId(*chunk_id));
                let mut tombstoned_now = false;
                if let Some(entry) = self.cluster_chunk_state.get_mut(&key) {
                    let was_tombstoned = entry.tombstoned;
                    entry.refcount = entry.refcount.saturating_sub(1);
                    if entry.refcount == 0 && !was_tombstoned {
                        entry.tombstoned = true;
                        tombstoned_now = true;
                    }
                }
                LogResponse::DecrementOutcome(tombstoned_now)
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

#[cfg(test)]
mod tests {
    /// Inline store key derivation: XOR sequence into last 8 bytes of `hashed_key`.
    /// Two deltas with the same `hashed_key` but different sequences must produce
    /// different inline keys (I-SF5 uniqueness invariant).
    fn compute_inline_key(hashed_key: &[u8; 32], sequence: u64) -> [u8; 32] {
        let mut k = *hashed_key;
        let seq_bytes = sequence.to_le_bytes();
        for (i, &b) in seq_bytes.iter().enumerate() {
            k[24 + i] ^= b;
        }
        k
    }

    #[test]
    fn inline_key_differs_for_different_sequences() {
        let hashed_key = [0xAB_u8; 32];
        let key_seq1 = compute_inline_key(&hashed_key, 1);
        let key_seq2 = compute_inline_key(&hashed_key, 2);
        assert_ne!(
            key_seq1, key_seq2,
            "inline keys for same hashed_key with different sequences must differ"
        );
    }

    #[test]
    fn inline_key_same_for_same_sequence() {
        let hashed_key = [0xCD_u8; 32];
        let key_a = compute_inline_key(&hashed_key, 42);
        let key_b = compute_inline_key(&hashed_key, 42);
        assert_eq!(
            key_a, key_b,
            "inline keys for same hashed_key and same sequence must be identical"
        );
    }

    #[test]
    fn inline_key_xor_only_affects_last_8_bytes() {
        let hashed_key = [0xFF_u8; 32];
        let key = compute_inline_key(&hashed_key, 1);
        // First 24 bytes should be unchanged.
        assert_eq!(&key[..24], &[0xFF_u8; 24]);
        // Last 8 bytes should differ from original (XOR with non-zero sequence).
        assert_ne!(&key[24..], &[0xFF_u8; 8]);
    }

    // -----------------------------------------------------------
    // Phase 16a — cluster_chunk_state Raft state machine.
    //
    // Tests the atomic CombinedProposal path that bundles a
    // composition delta with chunk metadata creation per D-4 + D-10
    // of specs/implementation/phase-16-cross-node-chunks.md.
    // -----------------------------------------------------------

    use super::*;
    use crate::raft_store::{LogCommand, NewChunkMeta};
    use kiseki_common::ids::{ChunkId, OrgId, ShardId};

    fn fresh_inner() -> ShardSmInner {
        ShardSmInner::new(
            ShardId(uuid::Uuid::from_u128(0xabc)),
            OrgId(uuid::Uuid::from_u128(0xdef)),
        )
    }

    fn org(b: u8) -> [u8; 16] {
        [b; 16]
    }

    fn chunk(b: u8) -> [u8; 32] {
        [b; 32]
    }

    /// Combined proposal: delta append + chunk meta create.
    /// Applying must produce BOTH a new delta AND a new
    /// `cluster_chunk_state` entry — atomically, in a single apply
    /// step. This is the I-L2 / I-L5 atomicity contract from D-4.
    #[test]
    fn combined_proposal_atomically_appends_delta_and_creates_chunk_meta() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let chunk_id = chunk(2);
        let cmd = LogCommand::ChunkAndDelta {
            tenant_id_bytes: tenant,
            operation: 0, // Create
            hashed_key: [0x55; 32],
            chunk_refs: vec![chunk_id],
            payload: vec![0xAA; 16],
            has_inline_data: false,
            new_chunks: vec![NewChunkMeta {
                chunk_id,
                placement: vec![1, 2, 3],
                original_len: 1024,
            }],
        };

        let _ = inner.apply_command(&cmd, 1);

        // Delta side observable.
        assert_eq!(inner.deltas.len(), 1, "delta must be appended");
        assert_eq!(inner.tip, 1);
        // Chunk meta side observable in the same apply step.
        let key = (OrgId(uuid::Uuid::from_bytes(tenant)), ChunkId(chunk_id));
        let entry = inner
            .cluster_chunk_state
            .get(&key)
            .expect("chunk_meta entry must exist after CombinedProposal apply");
        assert_eq!(entry.refcount, 1);
        assert_eq!(entry.placement, vec![1, 2, 3]);
        assert!(!entry.tombstoned);
        // Phase 16d step 3: original_len round-trips into the
        // cluster_chunk_state row so read_chunk_ec can decode
        // without the trim-trailing-zeros heuristic.
        assert_eq!(
            entry.original_len, 1024,
            "original_len must round-trip into cluster_chunk_state"
        );
    }

    /// Separate tenants writing the same `chunk_id` end up with
    /// independent `cluster_chunk_state` entries. Q1.C round-2 fix:
    /// `(tenant_id, chunk_id)` keying prevents the cross-tenant
    /// refcount inference leak under cross-tenant dedup.
    #[test]
    fn chunk_meta_keyed_by_tenant_isolates_refcount_across_tenants() {
        let mut inner = fresh_inner();
        let chunk_id = chunk(7);
        let tenant_a = org(1);
        let tenant_b = org(2);

        for tenant in [tenant_a, tenant_b] {
            let cmd = LogCommand::ChunkAndDelta {
                tenant_id_bytes: tenant,
                operation: 0,
                hashed_key: [0x33; 32],
                chunk_refs: vec![chunk_id],
                payload: vec![],
                has_inline_data: false,
                new_chunks: vec![NewChunkMeta {
                    chunk_id,
                    placement: vec![1, 2, 3],
                    original_len: 0,
                }],
            };
            let _ = inner.apply_command(&cmd, 1);
        }

        let key_a = (OrgId(uuid::Uuid::from_bytes(tenant_a)), ChunkId(chunk_id));
        let key_b = (OrgId(uuid::Uuid::from_bytes(tenant_b)), ChunkId(chunk_id));
        assert_eq!(
            inner.cluster_chunk_state.get(&key_a).map(|e| e.refcount),
            Some(1),
            "tenant A has its own refcount=1"
        );
        assert_eq!(
            inner.cluster_chunk_state.get(&key_b).map(|e| e.refcount),
            Some(1),
            "tenant B has its own refcount=1, independent of tenant A"
        );
    }

    /// `IncrementChunkRefcount` on an existing entry bumps refcount
    /// (used when a second composition references an already-stored
    /// chunk via dedup).
    #[test]
    fn increment_chunk_refcount_bumps_existing_entry() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let chunk_id = chunk(3);

        // Seed via combined proposal.
        let _ = inner.apply_command(
            &LogCommand::ChunkAndDelta {
                tenant_id_bytes: tenant,
                operation: 0,
                hashed_key: [0; 32],
                chunk_refs: vec![chunk_id],
                payload: vec![],
                has_inline_data: false,
                new_chunks: vec![NewChunkMeta {
                    chunk_id,
                    placement: vec![1, 2, 3],
                    original_len: 0,
                }],
            },
            1,
        );

        // Bump.
        let _ = inner.apply_command(
            &LogCommand::IncrementChunkRefcount {
                tenant_id_bytes: tenant,
                chunk_id,
            },
            2,
        );

        let key = (OrgId(uuid::Uuid::from_bytes(tenant)), ChunkId(chunk_id));
        assert_eq!(inner.cluster_chunk_state[&key].refcount, 2);
    }

    /// Decrement to refcount=0 tombstones the entry (does not
    /// remove it from the map immediately — compaction prunes
    /// tombstones later, per D-4 round 2).
    #[test]
    fn decrement_to_zero_tombstones_entry_for_compaction_prune() {
        let mut inner = fresh_inner();
        let tenant = org(1);
        let chunk_id = chunk(4);

        let _ = inner.apply_command(
            &LogCommand::ChunkAndDelta {
                tenant_id_bytes: tenant,
                operation: 0,
                hashed_key: [0; 32],
                chunk_refs: vec![chunk_id],
                payload: vec![],
                has_inline_data: false,
                new_chunks: vec![NewChunkMeta {
                    chunk_id,
                    placement: vec![1, 2, 3],
                    original_len: 0,
                }],
            },
            1,
        );
        let _ = inner.apply_command(
            &LogCommand::DecrementChunkRefcount {
                tenant_id_bytes: tenant,
                chunk_id,
            },
            2,
        );

        let key = (OrgId(uuid::Uuid::from_bytes(tenant)), ChunkId(chunk_id));
        let entry = inner
            .cluster_chunk_state
            .get(&key)
            .expect("entry still present (tombstoned, not removed)");
        assert_eq!(entry.refcount, 0);
        assert!(entry.tombstoned, "refcount=0 must mark entry tombstoned");
    }
}
