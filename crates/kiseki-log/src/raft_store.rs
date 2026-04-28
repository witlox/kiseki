//! Raft-ready log store — command-log state machine for shard operations.
//!
//! Each shard has its own command log and state machine. In production,
//! each shard's log is a separate Raft group. The state machine is
//! deterministic: given the same command sequence, it always reaches
//! the same state.

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};
use serde::{Deserialize, Serialize};

use crate::delta::{Delta, DeltaHeader, DeltaPayload};
use crate::error::LogError;
use crate::shard::{ShardConfig, ShardInfo, ShardState};
use crate::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};
use crate::watermark::ConsumerWatermarks;

/// Commands applied to a shard's state machine via the Raft log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum LogCommand {
    /// Append a delta to the shard.
    AppendDelta {
        /// Tenant ID.
        tenant_id_bytes: [u8; 16],
        /// Operation type code.
        operation: u8,
        /// Hashed key.
        hashed_key: [u8; 32],
        /// Chunk reference IDs.
        chunk_refs: Vec<[u8; 32]>,
        /// Encrypted payload.
        payload: Vec<u8>,
        /// Has inline data.
        has_inline_data: bool,
    },
    /// Atomic combined proposal — appends a delta AND creates one
    /// or more `cluster_chunk_state` entries in a single state-
    /// machine apply step. Used by Phase 16a's cross-node chunk
    /// write path: the gateway fans out fragments to peers (2-of-3
    /// quorum), then submits this combined proposal to Raft. On
    /// commit, every replica has both the composition delta AND
    /// the `cluster_chunk_state` entries.
    ///
    /// Atomicity is mandatory: applying this command must produce
    /// either both effects or neither. The gateway's client ack
    /// happens after the Raft commit of this command, NOT after
    /// the fragment fan-out. See specs/implementation/phase-16-
    /// cross-node-chunks.md D-4 + D-10 for the contract.
    ChunkAndDelta {
        /// Tenant ID (delta side).
        tenant_id_bytes: [u8; 16],
        /// Operation type code (delta side).
        operation: u8,
        /// Hashed key (delta side).
        hashed_key: [u8; 32],
        /// Chunk reference IDs (delta side).
        chunk_refs: Vec<[u8; 32]>,
        /// Encrypted payload (delta side).
        payload: Vec<u8>,
        /// Has inline data (delta side).
        has_inline_data: bool,
        /// Chunk metadata to create alongside the delta. Each entry
        /// becomes a `cluster_chunk_state` row keyed by
        /// `(tenant_id, chunk_id)`. Empty list means "delta only,
        /// no new chunks" (e.g., delete operation).
        new_chunks: Vec<NewChunkMeta>,
    },
    /// Increment the refcount of an existing `cluster_chunk_state`
    /// entry. Used when a second composition references an already-
    /// stored chunk via dedup (I-C2). No effect on deltas.
    IncrementChunkRefcount {
        /// Tenant scope (chunk-meta key).
        tenant_id_bytes: [u8; 16],
        /// Chunk identifier.
        chunk_id: [u8; 32],
    },
    /// Decrement the refcount of an existing `cluster_chunk_state`
    /// entry. When refcount reaches 0, the entry is *tombstoned*
    /// (kept in the state for compaction-time pruning) and the
    /// chunk's fragments become eligible for cluster-wide GC.
    DecrementChunkRefcount {
        /// Tenant scope (chunk-meta key).
        tenant_id_bytes: [u8; 16],
        /// Chunk identifier.
        chunk_id: [u8; 32],
    },
    /// Set maintenance mode.
    SetMaintenance {
        /// Whether to enable or disable.
        enabled: bool,
    },
    /// Advance a consumer watermark.
    AdvanceWatermark {
        /// Consumer name.
        consumer: String,
        /// New position.
        position: u64,
    },
}

/// New `cluster_chunk_state` entry to be created as part of a
/// `ChunkAndDelta` combined proposal. The entry's
/// `(tenant_id, chunk_id)` key is derived from the enclosing
/// command's tenant + this struct's `chunk_id`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NewChunkMeta {
    /// Content-addressed chunk identifier (32 bytes).
    pub chunk_id: [u8; 32],
    /// Node IDs holding fragments for this chunk. For Replication-3
    /// with 3 peers, all 3 node ids; under EC the placement list
    /// matches the CRUSH derivation in `kiseki-chunk::placement`.
    pub placement: Vec<u64>,
    /// Phase 16d step 3: pre-encode ciphertext length. Used by
    /// `ClusteredChunkStore::read_chunk_ec` to size the decoded
    /// output exactly; without this the read path falls back to a
    /// trim-trailing-zeros heuristic that's wrong for plaintext
    /// ending in zeros.
    #[serde(default)]
    pub original_len: u64,
}

impl std::fmt::Display for LogCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AppendDelta { operation, .. } => write!(f, "AppendDelta(op={operation})"),
            Self::ChunkAndDelta {
                operation,
                new_chunks,
                ..
            } => write!(
                f,
                "ChunkAndDelta(op={operation}, new_chunks={})",
                new_chunks.len()
            ),
            Self::IncrementChunkRefcount { .. } => write!(f, "IncrementChunkRefcount"),
            Self::DecrementChunkRefcount { .. } => write!(f, "DecrementChunkRefcount"),
            Self::SetMaintenance { enabled } => write!(f, "SetMaintenance({enabled})"),
            Self::AdvanceWatermark { consumer, position } => {
                write!(f, "AdvanceWatermark({consumer}={position})")
            }
        }
    }
}

/// Per-shard state machine.
struct ShardStateMachine {
    info: ShardInfo,
    deltas: Vec<Delta>,
    watermarks: ConsumerWatermarks,
    last_applied: u64,
}

/// Inner state: all shards.
struct Inner {
    shards: HashMap<ShardId, ShardStateMachine>,
    logs: HashMap<ShardId, Vec<(u64, LogCommand)>>,
}

/// Raft-ready log store with per-shard command logs.
pub struct RaftLogStore {
    inner: Mutex<Inner>,
}

impl RaftLogStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner {
                shards: HashMap::new(),
                logs: HashMap::new(),
            }),
        }
    }

    /// Create a new shard (called during namespace creation or split).
    pub fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let info = ShardInfo {
            shard_id,
            tenant_id,
            raft_members: vec![node_id],
            leader: Some(node_id),
            tip: SequenceNumber(0),
            delta_count: 0,
            byte_size: 0,
            state: ShardState::Healthy,
            config,
            range_start: [0u8; 32],
            range_end: [0xff; 32],
        };
        inner.shards.insert(
            shard_id,
            ShardStateMachine {
                info,
                deltas: Vec::new(),
                watermarks: ConsumerWatermarks::new(),
                last_applied: 0,
            },
        );
        inner.logs.insert(shard_id, Vec::new());
    }

    /// Register a consumer watermark.
    pub fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sm = inner
            .shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        sm.watermarks.register(consumer, position);
        Ok(())
    }

    /// Get the command log length for a shard.
    #[must_use]
    pub fn log_length(&self, shard_id: ShardId) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.logs.get(&shard_id).map_or(0, Vec::len)
    }

    /// Apply a command to a shard: append to log, apply to state machine.
    #[allow(clippy::needless_pass_by_value)]
    fn apply_command(&self, shard_id: ShardId, cmd: LogCommand) -> Result<(), LogError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let log = inner
            .logs
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        let index = log.len() as u64 + 1;
        log.push((index, cmd.clone()));

        let sm = inner
            .shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        Self::apply_to_sm(sm, shard_id, index, &cmd);
        Ok(())
    }

    /// Apply a single command to a shard state machine.
    #[allow(clippy::too_many_lines)] // big match per LogCommand variant
    fn apply_to_sm(sm: &mut ShardStateMachine, shard_id: ShardId, index: u64, cmd: &LogCommand) {
        if index <= sm.last_applied {
            return;
        }
        sm.last_applied = index;

        match cmd {
            LogCommand::AppendDelta {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
            } => {
                let next_seq = SequenceNumber(sm.info.tip.0 + 1);
                #[allow(clippy::cast_possible_truncation)]
                let payload_size = payload.len() as u32;

                let op = match operation {
                    0 => crate::delta::OperationType::Create,
                    1 => crate::delta::OperationType::Update,
                    2 => crate::delta::OperationType::Delete,
                    3 => crate::delta::OperationType::Rename,
                    4 => crate::delta::OperationType::SetAttribute,
                    _ => crate::delta::OperationType::Finalize,
                };

                // Construct a minimal timestamp for the state machine.
                let timestamp = kiseki_common::time::DeltaTimestamp {
                    hlc: kiseki_common::time::HybridLogicalClock {
                        physical_ms: index,
                        logical: 0,
                        node_id: NodeId(0),
                    },
                    wall: kiseki_common::time::WallTime {
                        millis_since_epoch: index,
                        timezone: "UTC".into(),
                    },
                    quality: kiseki_common::time::ClockQuality::Ntp,
                };

                let delta = Delta {
                    header: DeltaHeader {
                        sequence: next_seq,
                        shard_id,
                        tenant_id: kiseki_common::ids::OrgId(uuid::Uuid::from_bytes(
                            *tenant_id_bytes,
                        )),
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
                        ciphertext: payload.clone(),
                        auth_tag: Vec::new(),
                        nonce: Vec::new(),
                        system_epoch: None,
                        tenant_epoch: None,
                        tenant_wrapped_material: Vec::new(),
                    },
                };

                sm.info.tip = next_seq;
                sm.info.delta_count += 1;
                sm.info.byte_size += u64::from(payload_size) + 128;
                sm.deltas.push(delta);
            }
            LogCommand::ChunkAndDelta {
                tenant_id_bytes,
                operation,
                hashed_key,
                chunk_refs,
                payload,
                has_inline_data,
                new_chunks: _,
            } => {
                // The simple in-memory RaftLogStore doesn't track
                // cluster_chunk_state — that lives in the openraft
                // ShardStateMachine. For this fallback path we
                // treat ChunkAndDelta as a plain AppendDelta and
                // discard the new_chunks list. Production uses
                // the openraft-backed RaftShardStore which DOES
                // track cluster_chunk_state.
                let next_seq = SequenceNumber(sm.info.tip.0 + 1);
                #[allow(clippy::cast_possible_truncation)]
                let payload_size = payload.len() as u32;
                let op = match operation {
                    0 => crate::delta::OperationType::Create,
                    1 => crate::delta::OperationType::Update,
                    2 => crate::delta::OperationType::Delete,
                    3 => crate::delta::OperationType::Rename,
                    4 => crate::delta::OperationType::SetAttribute,
                    _ => crate::delta::OperationType::Finalize,
                };
                let timestamp = kiseki_common::time::DeltaTimestamp {
                    hlc: kiseki_common::time::HybridLogicalClock {
                        physical_ms: index,
                        logical: 0,
                        node_id: NodeId(0),
                    },
                    wall: kiseki_common::time::WallTime {
                        millis_since_epoch: index,
                        timezone: "UTC".into(),
                    },
                    quality: kiseki_common::time::ClockQuality::Ntp,
                };
                let delta = Delta {
                    header: DeltaHeader {
                        sequence: next_seq,
                        shard_id,
                        tenant_id: kiseki_common::ids::OrgId(uuid::Uuid::from_bytes(
                            *tenant_id_bytes,
                        )),
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
                        ciphertext: payload.clone(),
                        auth_tag: Vec::new(),
                        nonce: Vec::new(),
                        system_epoch: None,
                        tenant_epoch: None,
                        tenant_wrapped_material: Vec::new(),
                    },
                };
                sm.info.tip = next_seq;
                sm.info.delta_count += 1;
                sm.info.byte_size += u64::from(payload_size) + 128;
                sm.deltas.push(delta);
            }
            LogCommand::IncrementChunkRefcount { .. }
            | LogCommand::DecrementChunkRefcount { .. } => {
                // The simple in-memory RaftLogStore doesn't track
                // cluster_chunk_state. No-op here; production path
                // is the openraft-backed RaftShardStore.
            }
            LogCommand::SetMaintenance { enabled } => {
                sm.info.state = if *enabled {
                    ShardState::Maintenance
                } else {
                    ShardState::Healthy
                };
            }
            LogCommand::AdvanceWatermark { consumer, position } => {
                sm.watermarks.advance(consumer, SequenceNumber(*position));
            }
        }
    }
}

impl Default for RaftLogStore {
    fn default() -> Self {
        Self::new()
    }
}

fn op_to_u8(op: crate::delta::OperationType) -> u8 {
    match op {
        crate::delta::OperationType::Create => 0,
        crate::delta::OperationType::Update => 1,
        crate::delta::OperationType::Delete => 2,
        crate::delta::OperationType::Rename => 3,
        crate::delta::OperationType::SetAttribute => 4,
        crate::delta::OperationType::Finalize => 5,
    }
}

#[async_trait::async_trait]
impl LogOps for RaftLogStore {
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        // Pre-check state and key range.
        {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let sm = inner
                .shards
                .get(&req.shard_id)
                .ok_or(LogError::ShardNotFound(req.shard_id))?;

            match sm.info.state {
                ShardState::Maintenance | ShardState::Retiring => {
                    return Err(LogError::MaintenanceMode(req.shard_id));
                }
                ShardState::Election => return Err(LogError::LeaderUnavailable(req.shard_id)),
                ShardState::QuorumLost => return Err(LogError::QuorumLost(req.shard_id)),
                ShardState::Healthy | ShardState::Splitting | ShardState::Merging => {}
            }

            if req.hashed_key < sm.info.range_start || req.hashed_key >= sm.info.range_end {
                return Err(LogError::KeyOutOfRange(req.shard_id));
            }
        }

        let cmd = LogCommand::AppendDelta {
            tenant_id_bytes: *req.tenant_id.0.as_bytes(),
            operation: op_to_u8(req.operation),
            hashed_key: req.hashed_key,
            chunk_refs: req.chunk_refs.iter().map(|c| c.0).collect(),
            payload: req.payload,
            has_inline_data: req.has_inline_data,
        };

        self.apply_command(req.shard_id, cmd)?;

        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(inner.shards[&req.shard_id].info.tip)
    }

    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sm = inner
            .shards
            .get(&req.shard_id)
            .ok_or(LogError::ShardNotFound(req.shard_id))?;

        if req.from > req.to {
            return Err(LogError::InvalidRange(req.shard_id));
        }

        Ok(sm
            .deltas
            .iter()
            .filter(|d| d.header.sequence >= req.from && d.header.sequence <= req.to)
            .cloned()
            .collect())
    }

    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner
            .shards
            .get(&shard_id)
            .map(|sm| sm.info.clone())
            .ok_or(LogError::ShardNotFound(shard_id))
    }

    async fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        self.apply_command(shard_id, LogCommand::SetMaintenance { enabled })
    }

    async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sm = inner
            .shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        let gc_boundary = sm.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));
        sm.deltas.retain(|d| d.header.sequence >= gc_boundary);

        Ok(gc_boundary)
    }

    async fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sm = inner
            .shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        let before = sm.deltas.len() as u64;
        let gc_boundary = sm.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));

        let mut latest: HashMap<[u8; 32], &Delta> = HashMap::new();
        for delta in &sm.deltas {
            let entry = latest.entry(delta.header.hashed_key).or_insert(delta);
            if delta.header.sequence > entry.header.sequence {
                *entry = delta;
            }
        }

        let surviving: Vec<Delta> = latest
            .into_values()
            .filter(|d| !(d.header.tombstone && d.header.sequence < gc_boundary))
            .cloned()
            .collect();

        let after = surviving.len() as u64;
        sm.deltas = surviving;
        sm.deltas.sort_by_key(|d| d.header.sequence);
        sm.info.delta_count = after;

        Ok(before.saturating_sub(after))
    }

    fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
        Self::create_shard(self, shard_id, tenant_id, node_id, config);
    }

    fn update_shard_range(&self, shard_id: ShardId, range_start: [u8; 32], range_end: [u8; 32]) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sm) = inner.shards.get_mut(&shard_id) {
            sm.info.range_start = range_start;
            sm.info.range_end = range_end;
        }
    }

    fn set_shard_state(&self, shard_id: ShardId, state: ShardState) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sm) = inner.shards.get_mut(&shard_id) {
            sm.info.state = state;
        }
    }

    fn set_shard_config(&self, shard_id: ShardId, config: ShardConfig) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(sm) = inner.shards.get_mut(&shard_id) {
            sm.info.config = config;
        }
    }

    async fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sm = inner
            .shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        sm.watermarks.register(consumer, position);
        Ok(())
    }

    async fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let sm = inner
            .shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        sm.watermarks.advance(consumer, position);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::OperationType;

    fn test_shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

    fn make_req(shard_id: ShardId, key_byte: u8) -> AppendDeltaRequest {
        AppendDeltaRequest {
            shard_id,
            tenant_id: test_tenant(),
            operation: OperationType::Create,
            timestamp: kiseki_common::time::DeltaTimestamp {
                hlc: kiseki_common::time::HybridLogicalClock {
                    physical_ms: 1000,
                    logical: 0,
                    node_id: NodeId(1),
                },
                wall: kiseki_common::time::WallTime {
                    millis_since_epoch: 1000,
                    timezone: "UTC".into(),
                },
                quality: kiseki_common::time::ClockQuality::Ntp,
            },
            hashed_key: [key_byte; 32],
            chunk_refs: vec![],
            payload: vec![0xab; 64],
            has_inline_data: false,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn append_via_command_log() {
        let store = RaftLogStore::new();
        store.create_shard(
            test_shard(),
            test_tenant(),
            NodeId(1),
            ShardConfig::default(),
        );

        let seq = store.append_delta(make_req(test_shard(), 0x50)).await;
        assert!(seq.is_ok());
        assert_eq!(seq.unwrap_or_else(|_| unreachable!()), SequenceNumber(1));
        assert_eq!(store.log_length(test_shard()), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn total_order_via_command_log() {
        let store = RaftLogStore::new();
        store.create_shard(
            test_shard(),
            test_tenant(),
            NodeId(1),
            ShardConfig::default(),
        );

        for i in 0u8..5 {
            let key = (i * 20 + 10) % 255;
            store
                .append_delta(make_req(test_shard(), key))
                .await
                .unwrap_or_else(|_| unreachable!());
        }

        let deltas = store
            .read_deltas(ReadDeltasRequest {
                shard_id: test_shard(),
                from: SequenceNumber(1),
                to: SequenceNumber(5),
            })
            .await
            .unwrap_or_else(|_| unreachable!());

        assert_eq!(deltas.len(), 5);
        for (i, d) in deltas.iter().enumerate() {
            assert_eq!(d.header.sequence, SequenceNumber(i as u64 + 1));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn maintenance_via_command_log() {
        let store = RaftLogStore::new();
        store.create_shard(
            test_shard(),
            test_tenant(),
            NodeId(1),
            ShardConfig::default(),
        );

        store
            .set_maintenance(test_shard(), true)
            .await
            .unwrap_or_else(|_| unreachable!());

        let result = store.append_delta(make_req(test_shard(), 0x50)).await;
        assert!(result.is_err());

        let health = store
            .shard_health(test_shard())
            .await
            .unwrap_or_else(|_| unreachable!());
        assert_eq!(health.state, ShardState::Maintenance);
    }
}
