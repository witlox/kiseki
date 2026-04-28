//! Phase 16b step 1 — gateway-side `ChunkAndDelta` emission.
//!
//! Verifies that when a chunk is freshly written through the gateway,
//! the resulting log proposal is `ChunkAndDelta` (carrying the new
//! `cluster_chunk_state` row) rather than a plain `AppendDelta`.
//! Without this, the `cluster_chunk_state` Raft table stays empty and
//! cluster-wide GC + repair scrub have no metadata to operate on.
//!
//! These tests are RED before step 1 wires the gateway path.

use std::sync::{Arc, Mutex};

use kiseki_chunk::store::ChunkStore;
use kiseki_common::ids::{NamespaceId, NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_gateway::mem_gateway::InMemoryGateway;
use kiseki_gateway::ops::GatewayOps;
use kiseki_log::error::LogError;
use kiseki_log::shard::{ShardConfig, ShardInfo, ShardState};
use kiseki_log::traits::{
    AppendChunkAndDeltaRequest, AppendDeltaRequest, LogOps, ReadDeltasRequest,
};

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(1))
}

/// Records every `LogOps` proposal so the gateway flow can be asserted.
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
        Ok(vec![])
    }

    async fn shard_health(
        &self,
        _shard_id: ShardId,
    ) -> Result<ShardInfo, LogError> {
        Err(LogError::Unavailable)
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

    fn set_shard_state(&self, _shard_id: ShardId, _state: ShardState) {}

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

fn setup(log: Arc<dyn LogOps + Send + Sync>) -> InMemoryGateway {
    let mut compositions = CompositionStore::new().with_log(log);
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: test_shard(),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });

    let chunks = ChunkStore::new();
    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    InMemoryGateway::new(compositions, kiseki_chunk::arc_async(chunks), master_key)
}

/// RED test #1: a fresh write (no prior dedup) of a chunk-sized
/// payload must surface as a `ChunkAndDelta` proposal carrying the
/// new chunk's id in `new_chunks`. Today the gateway emits
/// `AppendDelta` so this fails GREEN until step 1 wires the path.
#[tokio::test(flavor = "multi_thread")]
async fn fresh_chunk_write_emits_chunk_and_delta_proposal() {
    let log = Arc::new(RecordingLog::default());
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>);

    // Use a payload large enough to force the chunk path (not inline).
    let plaintext = vec![0xABu8; 4096];
    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data: plaintext,
    })
    .await
    .expect("write");

    let chunk_calls = log.chunk_and_delta_calls.lock().unwrap();
    let plain_calls = log.plain_calls.lock().unwrap();
    assert_eq!(
        chunk_calls.len(),
        1,
        "fresh chunk write must produce exactly one ChunkAndDelta proposal"
    );
    assert!(
        plain_calls.is_empty(),
        "fresh chunk write must NOT take the plain AppendDelta path"
    );
    let proposal = &chunk_calls[0];
    assert_eq!(
        proposal.new_chunks.len(),
        1,
        "exactly one new chunk should be reported"
    );
    assert_eq!(
        proposal.delta.chunk_refs.len(),
        1,
        "delta must reference the new chunk"
    );
    assert_eq!(
        proposal.delta.chunk_refs[0].0,
        proposal.new_chunks[0].chunk_id,
        "chunk_refs and new_chunks must agree on the chunk id"
    );
}

/// RED test #2: a write that hits dedup (same plaintext as a prior
/// write) does NOT carry a `new_chunks` entry — the chunk already
/// exists, so we don't propose a new `cluster_chunk_state` row. The
/// proposal is either plain `AppendDelta` or `ChunkAndDelta` with
/// empty `new_chunks`. (Step 1 keeps it simple: plain `AppendDelta`
/// on dedup.)
#[tokio::test(flavor = "multi_thread")]
async fn dedup_write_does_not_emit_chunk_and_delta() {
    let log = Arc::new(RecordingLog::default());
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>);

    let plaintext = vec![0xCDu8; 4096];

    // First write: creates chunk + cluster_chunk_state.
    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data: plaintext.clone(),
    })
    .await
    .expect("write 1");

    // Second write of the SAME bytes: dedup hit.
    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data: plaintext,
    })
    .await
    .expect("write 2");

    let chunk_calls = log.chunk_and_delta_calls.lock().unwrap();
    let plain_calls = log.plain_calls.lock().unwrap();

    // First write goes ChunkAndDelta. Second write must NOT carry a
    // new chunk — that's a phantom cluster_chunk_state row (refcount
    // collision) which would corrupt dedup accounting.
    assert_eq!(
        chunk_calls.len(),
        1,
        "only the first write should propose a ChunkAndDelta with a new chunk; got {}",
        chunk_calls.len()
    );
    assert_eq!(
        plain_calls.len(),
        1,
        "the dedup-hit second write should take the plain AppendDelta path"
    );
}
