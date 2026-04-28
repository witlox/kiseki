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
use kiseki_chunk::{AsyncChunkOps, ChunkError};
use kiseki_common::ids::{ChunkId, NamespaceId, NodeId, OrgId, SequenceNumber, ShardId};
use kiseki_common::tenancy::KeyEpoch;
use kiseki_composition::composition::CompositionStore;
use kiseki_composition::namespace::Namespace;
use kiseki_crypto::keys::SystemMasterKey;
use kiseki_crypto::envelope::Envelope;
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
#[allow(clippy::struct_field_names)]
struct RecordingLog {
    plain_calls: Mutex<Vec<AppendDeltaRequest>>,
    chunk_and_delta_calls: Mutex<Vec<AppendChunkAndDeltaRequest>>,
    decrement_calls: Mutex<Vec<(ShardId, OrgId, ChunkId)>>,
    increment_calls: Mutex<Vec<(ShardId, OrgId, ChunkId)>>,
    /// Phase 16c: what the mock decrement should report. Tests that
    /// want to exercise the fan-out branch set this to `true`.
    tombstone_response: Mutex<bool>,
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

    async fn increment_chunk_refcount(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<(), LogError> {
        self.increment_calls
            .lock()
            .unwrap()
            .push((shard_id, tenant_id, chunk_id));
        Ok(())
    }

    async fn decrement_chunk_refcount(
        &self,
        shard_id: ShardId,
        tenant_id: OrgId,
        chunk_id: ChunkId,
    ) -> Result<bool, LogError> {
        self.decrement_calls
            .lock()
            .unwrap()
            .push((shard_id, tenant_id, chunk_id));
        // Test default: no tombstone. Specific tests override via a
        // separate field (see tombstone_responder below) when they
        // want the gateway to take the fan-out branch.
        Ok(*self.tombstone_response.lock().unwrap())
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
    setup_with_placement(log, vec![])
}

fn setup_with_placement(
    log: Arc<dyn LogOps + Send + Sync>,
    placement: Vec<u64>,
) -> InMemoryGateway {
    let n = placement.len();
    setup_with_placement_and_target(log, placement, n)
}

fn setup_with_placement_and_target(
    log: Arc<dyn LogOps + Send + Sync>,
    placement: Vec<u64>,
    target_copies: usize,
) -> InMemoryGateway {
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
        .with_cluster_placement(placement)
        .with_target_copies(target_copies)
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

// === Phase 16b step 2: placement plumbing + decrement on delete ===

/// RED: a gateway configured with a non-empty placement list must
/// surface that placement in the `ChunkAndDelta` proposal so the
/// `cluster_chunk_state[(tenant, chunk_id)]` row records who holds
/// the fragments — required by step 4's repair scrub and the
/// cross-cluster GC fan-out (step 2 follow-up).
#[tokio::test(flavor = "multi_thread")]
async fn fresh_chunk_write_carries_configured_placement() {
    let log = Arc::new(RecordingLog::default());
    let placement = vec![1u64, 2, 3];
    let gw = setup_with_placement(
        Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>,
        placement.clone(),
    );

    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data: vec![0xEEu8; 4096],
    })
    .await
    .expect("write");

    let calls = log.chunk_and_delta_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    let proposal = &calls[0];
    assert_eq!(
        proposal.new_chunks[0].placement, placement,
        "ChunkAndDelta must carry the configured cluster placement"
    );
}

/// Phase 16c step 2: when the cluster has more nodes than the
/// `target_copies` knob allows, the gateway must pick exactly
/// `target_copies` of them (via deterministic CRUSH-style hashing)
/// and put only those in `NewChunkMeta.placement`.
#[tokio::test(flavor = "multi_thread")]
async fn placement_is_capped_at_target_copies_when_cluster_is_larger() {
    let log = Arc::new(RecordingLog::default());
    // 6-node cluster, but Replication-3 ⇒ each chunk lives on 3.
    let gw = setup_with_placement_and_target(
        Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>,
        vec![1, 2, 3, 4, 5, 6],
        3,
    );

    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data: vec![0xC1u8; 4096],
    })
    .await
    .expect("write");

    let calls = log.chunk_and_delta_calls.lock().unwrap();
    let placement = &calls[0].new_chunks[0].placement;
    assert_eq!(
        placement.len(),
        3,
        "6-node cluster + target_copies=3 must yield exactly 3 placement entries; got {placement:?}"
    );
    // All entries must be from the cluster set.
    for n in placement {
        assert!(
            [1u64, 2, 3, 4, 5, 6].contains(n),
            "placement node {n} not in cluster"
        );
    }
}

/// RED: a gateway with an empty placement list (single-node mode)
/// emits `ChunkAndDelta` with empty placement — same as before this
/// step, but pinned now so a future regression doesn't accidentally
/// fill it with bogus values.
#[tokio::test(flavor = "multi_thread")]
async fn single_node_gateway_emits_empty_placement() {
    let log = Arc::new(RecordingLog::default());
    let gw = setup(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>);

    gw.write(kiseki_gateway::WriteRequest {
        tenant_id: test_tenant(),
        namespace_id: test_namespace(),
        data: vec![0x77u8; 4096],
    })
    .await
    .expect("write");

    let calls = log.chunk_and_delta_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].new_chunks[0].placement.is_empty(),
        "single-node gateway must emit empty placement"
    );
}

/// RED: composition delete must emit `decrement_chunk_refcount` for
/// every chunk the composition referenced. This is the cluster-wide
/// counterpart to the local refcount drop the gateway already does;
/// without it `cluster_chunk_state` never tombstones and step 4's
/// scrub has no signal to act on.
#[tokio::test(flavor = "multi_thread")]
async fn composition_delete_emits_decrement_for_each_chunk() {
    let log = Arc::new(RecordingLog::default());
    let gw = setup_with_placement(
        Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>,
        vec![1, 2, 3],
    );

    let resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: vec![0xAAu8; 4096],
        })
        .await
        .expect("write");

    // Sanity check that the write produced exactly one new chunk.
    let chunk_id = log.chunk_and_delta_calls.lock().unwrap()[0].new_chunks[0].chunk_id;

    gw.delete(test_tenant(), test_namespace(), resp.composition_id)
        .await
        .expect("delete");

    let dec_calls = log.decrement_calls.lock().unwrap();
    assert_eq!(
        dec_calls.len(),
        1,
        "composition delete must emit one decrement per referenced chunk"
    );
    let (shard, tenant, cid) = dec_calls[0];
    assert_eq!(shard, test_shard(), "shard id matches the composition");
    assert_eq!(tenant, test_tenant(), "tenant id matches");
    assert_eq!(cid.0, chunk_id, "decrement targets the right chunk");
}

// === Phase 16c step 1: DeleteFragment fan-out on tombstone ===============

/// `AsyncChunkOps` wrapper that records `delete_distributed` calls so
/// the gateway test can assert the tombstone branch was taken.
struct RecordingChunks {
    inner: Arc<dyn AsyncChunkOps>,
    fanout_calls: Mutex<Vec<(ChunkId, OrgId)>>,
}

impl RecordingChunks {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: kiseki_chunk::arc_async(ChunkStore::new()),
            fanout_calls: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait::async_trait]
impl AsyncChunkOps for RecordingChunks {
    async fn write_chunk(&self, env: Envelope, pool: &str) -> Result<bool, ChunkError> {
        self.inner.write_chunk(env, pool).await
    }
    async fn read_chunk(&self, id: &ChunkId) -> Result<Envelope, ChunkError> {
        self.inner.read_chunk(id).await
    }
    async fn increment_refcount(&self, id: &ChunkId) -> Result<u64, ChunkError> {
        self.inner.increment_refcount(id).await
    }
    async fn decrement_refcount(&self, id: &ChunkId) -> Result<u64, ChunkError> {
        self.inner.decrement_refcount(id).await
    }
    async fn set_retention_hold(
        &self,
        id: &ChunkId,
        hold: &str,
    ) -> Result<(), ChunkError> {
        self.inner.set_retention_hold(id, hold).await
    }
    async fn release_retention_hold(
        &self,
        id: &ChunkId,
        hold: &str,
    ) -> Result<(), ChunkError> {
        self.inner.release_retention_hold(id, hold).await
    }
    async fn gc(&self) -> u64 {
        self.inner.gc().await
    }
    async fn refcount(&self, id: &ChunkId) -> Result<u64, ChunkError> {
        self.inner.refcount(id).await
    }
    async fn delete_distributed(
        &self,
        chunk_id: &ChunkId,
        tenant_id: OrgId,
    ) -> Result<(), ChunkError> {
        self.fanout_calls.lock().unwrap().push((*chunk_id, tenant_id));
        Ok(())
    }
}

fn setup_with_chunks(
    log: Arc<dyn LogOps + Send + Sync>,
    chunks: Arc<dyn AsyncChunkOps>,
    placement: Vec<u64>,
) -> InMemoryGateway {
    let mut compositions = CompositionStore::new().with_log(log);
    compositions.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: test_shard(),
        read_only: false,
        versioning_enabled: false,
        compliance_tags: Vec::new(),
    });

    let master_key = SystemMasterKey::new([0x42; 32], KeyEpoch(1));
    InMemoryGateway::new(compositions, chunks, master_key).with_cluster_placement(placement)
}

/// RED: when the log reports the chunk's `cluster_chunk_state` row
/// transitioned to tombstoned (refcount→0), the gateway calls
/// `chunks.delete_distributed` exactly once for that chunk so the
/// leader's fan-out reclaims fragments on every peer.
#[tokio::test(flavor = "multi_thread")]
async fn tombstone_decrement_triggers_delete_distributed() {
    let log = Arc::new(RecordingLog::default());
    *log.tombstone_response.lock().unwrap() = true;
    let chunks = RecordingChunks::new();
    let gw = setup_with_chunks(
        Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>,
        Arc::clone(&chunks) as Arc<dyn AsyncChunkOps>,
        vec![1, 2, 3],
    );

    let resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: vec![0xAFu8; 4096],
        })
        .await
        .expect("write");
    let chunk_id = log.chunk_and_delta_calls.lock().unwrap()[0].new_chunks[0].chunk_id;

    gw.delete(test_tenant(), test_namespace(), resp.composition_id)
        .await
        .expect("delete");

    let fanouts = chunks.fanout_calls.lock().unwrap();
    assert_eq!(
        fanouts.len(),
        1,
        "tombstone signal must trigger exactly one fan-out"
    );
    assert_eq!(fanouts[0].0 .0, chunk_id);
    assert_eq!(fanouts[0].1, test_tenant());
}

/// RED: when the log reports `tombstoned=false` (another composition
/// still references the chunk), the gateway must NOT fan out — that
/// would reclaim a still-live chunk and break I-C2.
#[tokio::test(flavor = "multi_thread")]
async fn non_tombstone_decrement_does_not_fan_out() {
    let log = Arc::new(RecordingLog::default());
    // tombstone_response defaults to false → no fan-out expected.
    let chunks = RecordingChunks::new();
    let gw = setup_with_chunks(
        Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>,
        Arc::clone(&chunks) as Arc<dyn AsyncChunkOps>,
        vec![1, 2, 3],
    );

    let resp = gw
        .write(kiseki_gateway::WriteRequest {
            tenant_id: test_tenant(),
            namespace_id: test_namespace(),
            data: vec![0xB0u8; 4096],
        })
        .await
        .expect("write");

    gw.delete(test_tenant(), test_namespace(), resp.composition_id)
        .await
        .expect("delete");

    assert!(
        chunks.fanout_calls.lock().unwrap().is_empty(),
        "non-tombstone decrement must NOT trigger fan-out — would break I-C2",
    );
}
