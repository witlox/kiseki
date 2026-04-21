//! In-memory shard store — reference implementation of [`LogOps`].
//!
//! Demonstrates all Log semantics without Raft. Production use will
//! replace this with a Raft-backed store using `openraft`.
//!
//! Uses `Mutex` for interior mutability so that `LogOps` methods can
//! take `&self` (required for Raft-backed implementations where
//! mutations go through the consensus layer).

use std::collections::HashMap;
use std::sync::Mutex;

use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};

use crate::delta::{Delta, DeltaHeader, DeltaPayload};
use crate::error::LogError;
use crate::shard::{ShardConfig, ShardInfo, ShardState};
use crate::traits::{AppendDeltaRequest, LogOps, ReadDeltasRequest};
use crate::watermark::ConsumerWatermarks;

/// A single in-memory shard.
struct MemShard {
    info: ShardInfo,
    deltas: Vec<Delta>,
    watermarks: ConsumerWatermarks,
    /// Lowest sequence still stored (deltas below this were GC'd).
    gc_floor: SequenceNumber,
}

/// In-memory store managing multiple shards.
///
/// No Raft, no persistence — for unit testing and development.
/// Interior mutability via `Mutex` matches the `&self` `LogOps` trait.
pub struct MemShardStore {
    shards: Mutex<HashMap<ShardId, MemShard>>,
}

impl MemShardStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shards: Mutex::new(HashMap::new()),
        }
    }

    /// Create a new shard with the given parameters.
    pub fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
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
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Idempotent: don't overwrite if shard already exists (e.g., restored from redb).
        shards.entry(shard_id).or_insert(MemShard {
            info,
            deltas: Vec::new(),
            watermarks: ConsumerWatermarks::new(),
            gc_floor: SequenceNumber(0),
        });
    }

    /// Update a shard's key range (used during split).
    pub fn update_shard_range(
        &self,
        shard_id: ShardId,
        range_start: [u8; 32],
        range_end: [u8; 32],
    ) {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(shard) = shards.get_mut(&shard_id) {
            shard.info.range_start = range_start;
            shard.info.range_end = range_end;
        }
    }

    /// Register a consumer on a shard's watermark tracker.
    pub fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        shard.watermarks.register(consumer, position);
        Ok(())
    }

    /// Advance a consumer's watermark.
    pub fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        shard.watermarks.advance(consumer, position);
        Ok(())
    }

    /// Check if the shard should split based on its config (I-L6).
    #[must_use]
    pub fn should_split(&self, shard_id: ShardId) -> bool {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shards.get(&shard_id).is_some_and(|s| {
            s.info.delta_count >= s.info.config.max_delta_count
                || s.info.byte_size >= s.info.config.max_byte_size
        })
    }

    /// Perform a shard split at the midpoint of the key range.
    pub fn split_shard(
        &self,
        shard_id: ShardId,
        new_shard_id: ShardId,
        node_id: NodeId,
    ) -> Result<ShardId, LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        let mut midpoint = [0u8; 32];
        for (i, mid) in midpoint.iter_mut().enumerate() {
            *mid = shard.info.range_start[i] / 2 + shard.info.range_end[i] / 2;
        }

        let old_end = shard.info.range_end;
        shard.info.range_end = midpoint;

        let mut new_deltas = Vec::new();
        let mut old_deltas = Vec::new();
        for delta in shard.deltas.drain(..) {
            if delta.header.hashed_key >= midpoint {
                new_deltas.push(delta);
            } else {
                old_deltas.push(delta);
            }
        }

        shard.deltas = old_deltas;
        shard.info.delta_count = shard.deltas.len() as u64;
        shard.info.byte_size = shard
            .deltas
            .iter()
            .map(|d| u64::from(d.header.payload_size) + 128)
            .sum();

        let tenant_id = shard.info.tenant_id;
        let config = shard.info.config.clone();

        let new_info = ShardInfo {
            shard_id: new_shard_id,
            tenant_id,
            raft_members: vec![node_id],
            leader: Some(node_id),
            tip: SequenceNumber(new_deltas.len() as u64),
            delta_count: new_deltas.len() as u64,
            byte_size: new_deltas
                .iter()
                .map(|d| u64::from(d.header.payload_size) + 128)
                .sum(),
            state: ShardState::Healthy,
            config,
            range_start: midpoint,
            range_end: old_end,
        };

        shards.insert(
            new_shard_id,
            MemShard {
                info: new_info,
                deltas: new_deltas,
                watermarks: ConsumerWatermarks::new(),
                gc_floor: SequenceNumber(0),
            },
        );

        Ok(new_shard_id)
    }
}

impl Default for MemShardStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LogOps for MemShardStore {
    fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&req.shard_id)
            .ok_or(LogError::ShardNotFound(req.shard_id))?;

        match shard.info.state {
            ShardState::Maintenance => return Err(LogError::MaintenanceMode(req.shard_id)),
            ShardState::Election => return Err(LogError::LeaderUnavailable(req.shard_id)),
            ShardState::QuorumLost => return Err(LogError::QuorumLost(req.shard_id)),
            ShardState::Healthy | ShardState::Splitting => {}
        }

        if req.hashed_key < shard.info.range_start || req.hashed_key >= shard.info.range_end {
            return Err(LogError::KeyOutOfRange(req.shard_id));
        }

        let next_seq = SequenceNumber(shard.info.tip.0 + 1);
        #[allow(clippy::cast_possible_truncation)]
        let payload_size = req.payload.len() as u32;

        let delta = Delta {
            header: DeltaHeader {
                sequence: next_seq,
                shard_id: req.shard_id,
                tenant_id: req.tenant_id,
                operation: req.operation,
                timestamp: req.timestamp,
                hashed_key: req.hashed_key,
                tombstone: req.operation == crate::delta::OperationType::Delete,
                chunk_refs: req.chunk_refs,
                payload_size,
                has_inline_data: req.has_inline_data,
            },
            payload: DeltaPayload {
                ciphertext: req.payload,
                auth_tag: Vec::new(),
                nonce: Vec::new(),
                system_epoch: None,
                tenant_epoch: None,
                tenant_wrapped_material: Vec::new(),
            },
        };

        shard.info.tip = next_seq;
        shard.info.delta_count += 1;
        shard.info.byte_size += u64::from(payload_size) + 128;
        shard.deltas.push(delta);

        Ok(next_seq)
    }

    fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get(&req.shard_id)
            .ok_or(LogError::ShardNotFound(req.shard_id))?;

        if req.from > req.to {
            return Err(LogError::InvalidRange(req.shard_id));
        }

        Ok(shard
            .deltas
            .iter()
            .filter(|d| d.header.sequence >= req.from && d.header.sequence <= req.to)
            .cloned()
            .collect())
    }

    fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shards
            .get(&shard_id)
            .map(|s| s.info.clone())
            .ok_or(LogError::ShardNotFound(shard_id))
    }

    fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;
        shard.info.state = if enabled {
            ShardState::Maintenance
        } else {
            ShardState::Healthy
        };
        Ok(())
    }

    fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        let gc_boundary = shard.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));
        shard.deltas.retain(|d| d.header.sequence >= gc_boundary);
        shard.gc_floor = gc_boundary;

        Ok(gc_boundary)
    }

    fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let shard = shards
            .get_mut(&shard_id)
            .ok_or(LogError::ShardNotFound(shard_id))?;

        let before_count = shard.deltas.len() as u64;
        let gc_boundary = shard.watermarks.gc_boundary().unwrap_or(SequenceNumber(0));

        let mut latest: std::collections::HashMap<[u8; 32], &Delta> =
            std::collections::HashMap::new();
        for delta in &shard.deltas {
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

        let after_count = surviving.len() as u64;
        shard.deltas = surviving;
        shard.deltas.sort_by_key(|d| d.header.sequence);
        shard.info.delta_count = after_count;

        Ok(before_count.saturating_sub(after_count))
    }
}
