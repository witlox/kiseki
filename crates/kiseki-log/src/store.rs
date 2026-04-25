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

    /// Update a shard's split thresholds (for testing auto-split).
    pub fn set_shard_config(&self, shard_id: ShardId, config: ShardConfig) {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(shard) = shards.get_mut(&shard_id) {
            shard.info.config = config;
        }
    }

    /// Set a shard's lifecycle state (ADR-034: merge state transitions).
    pub fn set_shard_state(&self, shard_id: ShardId, state: ShardState) {
        let mut shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(shard) = shards.get_mut(&shard_id) {
            shard.info.state = state;
        }
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

#[async_trait::async_trait]
impl LogOps for MemShardStore {
    async fn append_delta(&self, req: AppendDeltaRequest) -> Result<SequenceNumber, LogError> {
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
            ShardState::Retiring => return Err(LogError::MaintenanceMode(req.shard_id)),
            ShardState::Healthy | ShardState::Splitting | ShardState::Merging => {}
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

    async fn read_deltas(&self, req: ReadDeltasRequest) -> Result<Vec<Delta>, LogError> {
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

    async fn shard_health(&self, shard_id: ShardId) -> Result<ShardInfo, LogError> {
        let shards = self
            .shards
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        shards
            .get(&shard_id)
            .map(|s| s.info.clone())
            .ok_or(LogError::ShardNotFound(shard_id))
    }

    async fn set_maintenance(&self, shard_id: ShardId, enabled: bool) -> Result<(), LogError> {
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

    async fn truncate_log(&self, shard_id: ShardId) -> Result<SequenceNumber, LogError> {
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

    async fn compact_shard(&self, shard_id: ShardId) -> Result<u64, LogError> {
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

    fn create_shard(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        node_id: NodeId,
        config: ShardConfig,
    ) {
        // Delegate to inherent method.
        Self::create_shard(self, shard_id, tenant_id, node_id, config);
    }

    fn update_shard_range(
        &self,
        shard_id: ShardId,
        range_start: [u8; 32],
        range_end: [u8; 32],
    ) {
        Self::update_shard_range(self, shard_id, range_start, range_end);
    }

    fn set_shard_state(&self, shard_id: ShardId, state: ShardState) {
        Self::set_shard_state(self, shard_id, state);
    }

    fn set_shard_config(&self, shard_id: ShardId, config: ShardConfig) {
        Self::set_shard_config(self, shard_id, config);
    }

    async fn register_consumer(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        Self::register_consumer(self, shard_id, consumer, position)
    }

    async fn advance_watermark(
        &self,
        shard_id: ShardId,
        consumer: &str,
        position: SequenceNumber,
    ) -> Result<(), LogError> {
        Self::advance_watermark(self, shard_id, consumer, position)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::OperationType;
    use kiseki_common::ids::{NodeId, OrgId, SequenceNumber, ShardId};
    use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};

    fn test_timestamp() -> DeltaTimestamp {
        DeltaTimestamp {
            hlc: HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: NodeId(1),
            },
            wall: WallTime {
                millis_since_epoch: 1000,
                timezone: "UTC".into(),
            },
            quality: ClockQuality::Ntp,
        }
    }

    // --- log.feature @unit: "Maintenance mode rejects writes" ---

    #[tokio::test]
    async fn maintenance_mode_rejects_writes() {
        let store = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_id, node_id, ShardConfig::default());

        // Enter maintenance mode.
        store.set_maintenance(shard_id, true).await.unwrap();

        // Verify shard is in maintenance state.
        let info = store.shard_health(shard_id).await.unwrap();
        assert_eq!(info.state, ShardState::Maintenance);

        // AppendDelta should be rejected with MaintenanceMode error.
        let req = AppendDeltaRequest {
            shard_id,
            tenant_id,
            operation: OperationType::Create,
            timestamp: test_timestamp(),
            hashed_key: [0x10u8; 32],
            chunk_refs: vec![],
            payload: vec![0xAA; 100],
            has_inline_data: false,
        };
        let result = store.append_delta(req).await;
        assert!(
            matches!(result, Err(crate::error::LogError::MaintenanceMode(_))),
            "writes must be rejected in maintenance mode"
        );

        // ReadDeltas should continue to work.
        let read_result = store
            .read_deltas(crate::traits::ReadDeltasRequest {
                shard_id,
                from: SequenceNumber(0),
                to: SequenceNumber(100),
            })
            .await;
        assert!(
            read_result.is_ok(),
            "reads must continue in maintenance mode"
        );

        // ShardHealth should continue to work.
        let health_result = store.shard_health(shard_id).await;
        assert!(
            health_result.is_ok(),
            "health queries must continue in maintenance mode"
        );
    }

    // --- log.feature @unit: "Exiting maintenance mode resumes writes" ---

    #[tokio::test]
    async fn exiting_maintenance_resumes_writes() {
        let store = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_id, node_id, ShardConfig::default());

        // Enter then exit maintenance mode.
        store.set_maintenance(shard_id, true).await.unwrap();
        store.set_maintenance(shard_id, false).await.unwrap();

        // Verify shard is healthy again.
        let info = store.shard_health(shard_id).await.unwrap();
        assert_eq!(info.state, ShardState::Healthy);

        // AppendDelta should be accepted again.
        let req = AppendDeltaRequest {
            shard_id,
            tenant_id,
            operation: OperationType::Create,
            timestamp: test_timestamp(),
            hashed_key: [0x10u8; 32],
            chunk_refs: vec![],
            payload: vec![0xAA; 100],
            has_inline_data: false,
        };
        let result = store.append_delta(req).await;
        assert!(
            result.is_ok(),
            "writes must resume after maintenance clears"
        );
    }

    // --- log.feature @unit: "Stream processor reads delta range" ---

    #[tokio::test]
    async fn stream_processor_reads_delta_range() {
        let store = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_id, node_id, ShardConfig::default());

        // Append 50 deltas (seq 1..=50).
        for i in 0u8..50 {
            let req = AppendDeltaRequest {
                shard_id,
                tenant_id,
                operation: OperationType::Create,
                timestamp: test_timestamp(),
                hashed_key: [i; 32],
                chunk_refs: vec![],
                payload: vec![0xAA; 64],
                has_inline_data: false,
            };
            store.append_delta(req).await.unwrap();
        }

        // Read range [40, 50] — simulating a stream processor reading
        // from position 40 to 50.
        let deltas = store
            .read_deltas(crate::traits::ReadDeltasRequest {
                shard_id,
                from: SequenceNumber(40),
                to: SequenceNumber(50),
            })
            .await
            .unwrap();

        // Should receive 11 deltas [40..=50] in order.
        assert_eq!(deltas.len(), 11);
        for (i, delta) in deltas.iter().enumerate() {
            assert_eq!(
                delta.header.sequence,
                SequenceNumber(40 + i as u64),
                "deltas must be in order"
            );
            // Each delta includes the full envelope (header + encrypted payload).
            assert!(
                !delta.payload.ciphertext.is_empty(),
                "payload must be present"
            );
        }
    }

    // --- log.feature @unit: "Phase marker { checkpoint } may inform compaction pacing" ---
    // The log works correctly regardless of advisory state. Phase markers
    // are MAY heuristics — they never affect delta ordering, durability,
    // or GC correctness (I-WA1). This test proves compaction works
    // without any advisory signal.

    #[tokio::test]
    async fn compaction_works_without_advisory_phase_markers() {
        let store = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_id, node_id, ShardConfig::default());

        // Append multiple versions of the same key (simulating checkpoint burst).
        for i in 0..5 {
            let req = AppendDeltaRequest {
                shard_id,
                tenant_id,
                operation: if i == 0 {
                    OperationType::Create
                } else {
                    OperationType::Update
                },
                timestamp: test_timestamp(),
                hashed_key: [0xAA; 32], // same key
                chunk_refs: vec![],
                payload: vec![0xBB; 100],
                has_inline_data: false,
            };
            store.append_delta(req).await.unwrap();
        }

        // Compact without any advisory/phase-marker signal.
        // Compaction MUST honour its configured thresholds regardless of hints (I-L6).
        let removed = store.compact_shard(shard_id).await.unwrap();
        assert!(
            removed > 0,
            "compaction must remove superseded deltas without advisory"
        );

        // Delta ordering is preserved after compaction.
        let remaining = store
            .read_deltas(crate::traits::ReadDeltasRequest {
                shard_id,
                from: SequenceNumber(0),
                to: SequenceNumber(u64::MAX),
            })
            .await
            .unwrap();
        assert_eq!(remaining.len(), 1, "only latest version should survive");
    }

    // --- log.feature @unit: "Shard saturation telemetry is caller-scoped" ---
    // The log produces per-shard metrics. This test verifies that shard
    // health (the basis for backpressure signals) is available per-shard
    // and reports the shard's own metrics independently.

    #[tokio::test]
    async fn shard_health_reports_independent_metrics() {
        let store = MemShardStore::new();
        let tenant_a = OrgId(uuid::Uuid::from_u128(100));
        let _tenant_b = OrgId(uuid::Uuid::from_u128(200));
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_a, node_id, ShardConfig::default());

        // Append deltas from tenant_a.
        for _ in 0..5 {
            let req = AppendDeltaRequest {
                shard_id,
                tenant_id: tenant_a,
                operation: OperationType::Create,
                timestamp: test_timestamp(),
                hashed_key: [0x10; 32],
                chunk_refs: vec![],
                payload: vec![0xAA; 100],
                has_inline_data: false,
            };
            store.append_delta(req).await.unwrap();
        }

        // Shard health reports metrics for the shard (basis for
        // caller-scoped telemetry at the gateway level).
        let info = store.shard_health(shard_id).await.unwrap();
        assert_eq!(info.delta_count, 5);
        assert!(info.byte_size > 0);

        // Requesting health for a nonexistent shard returns an error
        // (same shape — I-WA6).
        let nonexistent = ShardId(uuid::Uuid::from_u128(999));
        let result = store.shard_health(nonexistent).await;
        assert!(
            result.is_err(),
            "nonexistent shard must return the same error shape"
        );
    }

    // --- log.feature @unit: "Advisory disabled — log serves all tenants normally" ---
    // When advisory is disabled cluster-wide, all Log operations succeed
    // with full correctness and durability (I-WA2). No compaction pacing
    // heuristic uses absent advisory signals.

    #[tokio::test]
    async fn advisory_disabled_log_operates_normally() {
        let store = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_id, node_id, ShardConfig::default());

        // Append deltas — no advisory signals present.
        for i in 0u8..10 {
            let req = AppendDeltaRequest {
                shard_id,
                tenant_id,
                operation: OperationType::Create,
                timestamp: test_timestamp(),
                hashed_key: [i; 32],
                chunk_refs: vec![],
                payload: vec![0xCC; 50],
                has_inline_data: false,
            };
            store.append_delta(req).await.unwrap();
        }

        // Read deltas — all 10 present.
        let deltas = store
            .read_deltas(crate::traits::ReadDeltasRequest {
                shard_id,
                from: SequenceNumber(0),
                to: SequenceNumber(u64::MAX),
            })
            .await
            .unwrap();
        assert_eq!(deltas.len(), 10);

        // Compact — works without advisory.
        let removed = store.compact_shard(shard_id).await.unwrap();
        // All keys are unique, so nothing is removed.
        assert_eq!(removed, 0);

        // Truncation works without advisory.
        store
            .register_consumer(shard_id, "sp-nfs", SequenceNumber(5))
            .unwrap();
        let boundary = store.truncate_log(shard_id).await.unwrap();
        assert_eq!(boundary, SequenceNumber(5));

        // Shard health is available.
        let info = store.shard_health(shard_id).await.unwrap();
        assert_eq!(info.state, ShardState::Healthy);
    }

    /// Inline threshold changes are prospective only (I-L9): the Log layer
    /// (`append_delta`) accepts payloads of any size. Threshold enforcement
    /// happens at the Gateway, not the Log. This test proves that both a
    /// 4 KB and an 8 KB delta succeed regardless of the shard's configured
    /// inline threshold (default 4096 bytes).
    #[tokio::test]
    async fn append_delta_accepts_any_payload_size() {
        let store = MemShardStore::new();
        let shard_id = ShardId(uuid::Uuid::from_u128(1));
        let tenant_id = OrgId(uuid::Uuid::from_u128(100));
        let node_id = NodeId(1);

        store.create_shard(shard_id, tenant_id, node_id, ShardConfig::default());

        let hashed_key = [0x10u8; 32]; // within default range [0x00, 0xff]

        // 4 KB payload — below default inline threshold (4096).
        let payload_4k = vec![0xAA; 4096];
        let req_4k = AppendDeltaRequest {
            shard_id,
            tenant_id,
            operation: OperationType::Create,
            timestamp: test_timestamp(),
            hashed_key,
            chunk_refs: vec![],
            payload: payload_4k,
            has_inline_data: true,
        };
        let seq1 = store
            .append_delta(req_4k)
            .await
            .expect("4KB delta should succeed");
        assert_eq!(seq1, SequenceNumber(1));

        // 8 KB payload — above default inline threshold (4096).
        let payload_8k = vec![0xBB; 8192];
        let req_8k = AppendDeltaRequest {
            shard_id,
            tenant_id,
            operation: OperationType::Update,
            timestamp: test_timestamp(),
            hashed_key,
            chunk_refs: vec![],
            payload: payload_8k,
            has_inline_data: false,
        };
        let seq2 = store
            .append_delta(req_8k)
            .await
            .expect("8KB delta should succeed");
        assert_eq!(seq2, SequenceNumber(2));
    }
}
