//! Bridge between Composition and Log contexts.
//!
//! When attached to a `CompositionStore`, composition mutations
//! (create, update, delete) emit deltas to the log shard. This
//! wires the Composition → Log data path per api-contracts.md.

use kiseki_common::ids::{ChunkId, NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_common::time::{ClockQuality, DeltaTimestamp, HybridLogicalClock, WallTime};
use kiseki_log::delta::OperationType;
use kiseki_log::error::LogError;
use kiseki_log::raft_store::NewChunkMeta;
use kiseki_log::traits::{AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps};

/// Emit a delta to the log for a composition mutation.
///
/// Called by `CompositionStore` after a successful create/update/delete.
/// The payload is the composition ID serialized as bytes (opaque to
/// the log per I-L7).
///
/// Returns the assigned `SequenceNumber` on success, or the `LogError`
/// on failure (e.g., `KeyOutOfRange` for stale shard map routing).
pub async fn emit_delta<L: LogOps + ?Sized>(
    log: &L,
    shard_id: ShardId,
    tenant_id: OrgId,
    operation: OperationType,
    hashed_key: [u8; 32],
    chunk_refs: Vec<ChunkId>,
    payload: Vec<u8>,
) -> Result<SequenceNumber, LogError> {
    let timestamp = now_timestamp();
    let req = AppendDeltaRequest {
        shard_id,
        tenant_id,
        operation,
        timestamp,
        hashed_key,
        chunk_refs,
        payload,
        has_inline_data: false,
    };
    log.append_delta(req).await
}

/// Phase 16b D-4: emit a `ChunkAndDelta` proposal so the
/// `cluster_chunk_state` rows for the new chunks land in the per-shard
/// Raft state machine atomically with the delta itself. Falls back to
/// plain `append_delta` when `new_chunks` is empty.
#[allow(clippy::too_many_arguments)]
pub async fn emit_chunk_and_delta<L: LogOps + ?Sized>(
    log: &L,
    shard_id: ShardId,
    tenant_id: OrgId,
    operation: OperationType,
    hashed_key: [u8; 32],
    chunk_refs: Vec<ChunkId>,
    payload: Vec<u8>,
    new_chunks: Vec<NewChunkMeta>,
) -> Result<SequenceNumber, LogError> {
    let timestamp = now_timestamp();
    let delta = AppendDeltaRequest {
        shard_id,
        tenant_id,
        operation,
        timestamp,
        hashed_key,
        chunk_refs,
        payload,
        has_inline_data: false,
    };
    if new_chunks.is_empty() {
        log.append_delta(delta).await
    } else {
        log.append_chunk_and_delta(AppendChunkAndDeltaRequest { delta, new_chunks })
            .await
    }
}

/// Monotonic logical counter for HLC tie-breaking (PIPE-ADV-2).
static HLC_LOGICAL: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn now_timestamp() -> DeltaTimestamp {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
    let logical = HLC_LOGICAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    DeltaTimestamp {
        hlc: HybridLogicalClock {
            physical_ms: now_ms,
            logical,
            node_id: NodeId(0),
        },
        wall: WallTime {
            millis_since_epoch: now_ms,
            timezone: "UTC".into(),
        },
        quality: ClockQuality::Ntp,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use kiseki_log::raft_store::NewChunkMeta;
    use kiseki_log::shard::{ShardConfig, ShardInfo};
    use kiseki_log::traits::{
        AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps, ReadDeltasRequest,
    };

    use super::*;

    /// Records every `LogOps` call so tests can assert which path was
    /// taken. Returns `LogError::Unavailable` from any read/admin
    /// methods we don't care about — the gateway write path only
    /// touches `append_delta` / `append_chunk_and_delta`.
    #[derive(Default)]
    struct RecordingLog {
        plain_calls: Mutex<Vec<AppendDeltaRequest>>,
        chunk_and_delta_calls: Mutex<Vec<AppendChunkAndDeltaRequest>>,
    }

    #[async_trait::async_trait]
    impl LogOps for RecordingLog {
        async fn append_delta(
            &self,
            req: AppendDeltaRequest,
        ) -> Result<SequenceNumber, LogError> {
            self.plain_calls.lock().unwrap().push(req);
            Ok(SequenceNumber(1))
        }

        async fn append_chunk_and_delta(
            &self,
            req: AppendChunkAndDeltaRequest,
        ) -> Result<SequenceNumber, LogError> {
            self.chunk_and_delta_calls.lock().unwrap().push(req);
            Ok(SequenceNumber(1))
        }

        async fn read_deltas(
            &self,
            _req: ReadDeltasRequest,
        ) -> Result<Vec<kiseki_log::delta::Delta>, LogError> {
            Err(LogError::Unavailable)
        }

        async fn shard_health(
            &self,
            shard_id: ShardId,
        ) -> Result<ShardInfo, LogError> {
            Err(LogError::ShardNotFound(shard_id))
        }

        async fn set_maintenance(
            &self,
            _shard_id: ShardId,
            _enabled: bool,
        ) -> Result<(), LogError> {
            Ok(())
        }

        async fn truncate_log(
            &self,
            _shard_id: ShardId,
        ) -> Result<SequenceNumber, LogError> {
            Ok(SequenceNumber(0))
        }

        async fn compact_shard(&self, _shard_id: ShardId) -> Result<u64, LogError> {
            Ok(0)
        }

        fn create_shard(
            &self,
            _shard_id: ShardId,
            _tenant_id: OrgId,
            _node_id: NodeId,
            _config: ShardConfig,
        ) {
        }

        fn update_shard_range(
            &self,
            _shard_id: ShardId,
            _range_start: [u8; 32],
            _range_end: [u8; 32],
        ) {
        }

        fn set_shard_state(&self, _shard_id: ShardId, _state: kiseki_log::shard::ShardState) {}

        fn set_shard_config(&self, _shard_id: ShardId, _config: ShardConfig) {}

        async fn register_consumer(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: SequenceNumber,
        ) -> Result<(), LogError> {
            Ok(())
        }

        async fn advance_watermark(
            &self,
            _shard_id: ShardId,
            _consumer: &str,
            _position: SequenceNumber,
        ) -> Result<(), LogError> {
            Ok(())
        }
    }

    fn shard() -> ShardId {
        ShardId(uuid::Uuid::from_u128(1))
    }

    fn tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(2))
    }

    /// Phase 16b step 1: when no new chunks are involved (e.g. an
    /// inline-only delta or a metadata-only update) the bridge takes
    /// the plain `append_delta` path — no `cluster_chunk_state`
    /// proposal is wasted.
    #[tokio::test]
    async fn empty_new_chunks_takes_plain_append_delta_path() {
        let log = RecordingLog::default();
        emit_chunk_and_delta(
            &log,
            shard(),
            tenant(),
            OperationType::Create,
            [0xAA; 32],
            vec![],
            b"payload".to_vec(),
            vec![],
        )
        .await
        .expect("ok");
        assert_eq!(
            log.plain_calls.lock().unwrap().len(),
            1,
            "empty new_chunks must take the plain path"
        );
        assert!(
            log.chunk_and_delta_calls.lock().unwrap().is_empty(),
            "empty new_chunks must NOT take the chunk-and-delta path"
        );
    }

    /// Phase 16b step 1, D-4: when new chunks are listed the bridge
    /// emits a single `ChunkAndDelta` proposal so the
    /// `cluster_chunk_state` row and the delta land atomically. The
    /// `new_chunks` payload is preserved end-to-end.
    #[tokio::test]
    async fn non_empty_new_chunks_takes_chunk_and_delta_path() {
        let log = RecordingLog::default();
        let chunk_a = ChunkId([0xC1; 32]);
        let chunk_b = ChunkId([0xC2; 32]);
        let new_chunks = vec![
            NewChunkMeta {
                chunk_id: chunk_a.0,
                placement: vec![1, 2, 3],
            },
            NewChunkMeta {
                chunk_id: chunk_b.0,
                placement: vec![1, 2, 3],
            },
        ];

        emit_chunk_and_delta(
            &log,
            shard(),
            tenant(),
            OperationType::Create,
            [0x77; 32],
            vec![chunk_a, chunk_b],
            b"payload".to_vec(),
            new_chunks.clone(),
        )
        .await
        .expect("ok");

        assert!(
            log.plain_calls.lock().unwrap().is_empty(),
            "non-empty new_chunks must NOT take the plain path"
        );
        let calls = log.chunk_and_delta_calls.lock().unwrap();
        assert_eq!(calls.len(), 1, "exactly one ChunkAndDelta proposal");
        let got = &calls[0];
        assert_eq!(got.delta.chunk_refs, vec![chunk_a, chunk_b]);
        assert_eq!(got.new_chunks.len(), 2);
        assert_eq!(got.new_chunks[0].chunk_id, chunk_a.0);
        assert_eq!(got.new_chunks[1].chunk_id, chunk_b.0);
    }
}
