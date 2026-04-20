//! End-to-end pipeline test: Composition → Log → View.
//!
//! Verifies that composition mutations emit deltas to the log,
//! and the stream processor advances view watermarks accordingly.

use std::sync::Arc;

use kiseki_common::ids::{ChunkId, NamespaceId, NodeId, OrgId, SequenceNumber, ShardId, ViewId};
use kiseki_composition::composition::{CompositionOps, CompositionStore};
use kiseki_composition::namespace::Namespace;
use kiseki_log::shard::ShardConfig;
use kiseki_log::store::MemShardStore;
use kiseki_log::traits::{LogOps, ReadDeltasRequest};
use kiseki_view::descriptor::{ConsistencyModel, ProtocolSemantics, ViewDescriptor};
use kiseki_view::stream_processor::TrackedStreamProcessor;
use kiseki_view::view::{ViewOps, ViewState, ViewStore};

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(1))
}

fn test_namespace() -> NamespaceId {
    NamespaceId(uuid::Uuid::from_u128(200))
}

fn test_view() -> ViewId {
    ViewId(uuid::Uuid::from_u128(300))
}

/// Set up the full pipeline: log store + composition store (with log bridge) + view store.
fn setup() -> (Arc<MemShardStore>, CompositionStore, ViewStore) {
    let log = Arc::new(MemShardStore::new());
    log.create_shard(
        test_shard(),
        test_tenant(),
        NodeId(1),
        ShardConfig::default(),
    );

    let compositions =
        CompositionStore::new().with_log(Arc::clone(&log) as Arc<dyn LogOps + Send + Sync>);
    let mut comp_store = compositions;
    comp_store.add_namespace(Namespace {
        id: test_namespace(),
        tenant_id: test_tenant(),
        shard_id: test_shard(),
        read_only: false,
    });

    let mut views = ViewStore::new();
    views
        .create_view(ViewDescriptor {
            view_id: test_view(),
            tenant_id: test_tenant(),
            source_shards: vec![test_shard()],
            protocol: ProtocolSemantics::Posix,
            consistency: ConsistencyModel::ReadYourWrites,
            discardable: true,
            version: 1,
        })
        .unwrap();

    (log, comp_store, views)
}

#[test]
fn create_composition_emits_delta_to_log() {
    let (log, mut comp, _views) = setup();

    // Create a composition.
    let _comp_id = comp
        .create(test_namespace(), vec![ChunkId([0x01; 32])], 1024)
        .unwrap();

    // The log should have one delta.
    let deltas = log
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(100),
        })
        .unwrap();

    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].header.sequence, SequenceNumber(1));
    assert_eq!(
        deltas[0].header.operation,
        kiseki_log::delta::OperationType::Create
    );
    assert_eq!(deltas[0].header.tenant_id, test_tenant());
}

#[test]
fn update_and_delete_emit_deltas() {
    let (log, mut comp, _views) = setup();

    let comp_id = comp
        .create(test_namespace(), vec![ChunkId([0x01; 32])], 100)
        .unwrap();

    comp.update(comp_id, vec![ChunkId([0x02; 32])], 200)
        .unwrap();

    comp.delete(comp_id).unwrap();

    let deltas = log
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(100),
        })
        .unwrap();

    assert_eq!(deltas.len(), 3);
    assert_eq!(
        deltas[0].header.operation,
        kiseki_log::delta::OperationType::Create
    );
    assert_eq!(
        deltas[1].header.operation,
        kiseki_log::delta::OperationType::Update
    );
    assert_eq!(
        deltas[2].header.operation,
        kiseki_log::delta::OperationType::Delete
    );
}

#[test]
fn stream_processor_advances_view_watermark() {
    let (log, mut comp, mut views) = setup();

    // Write 3 compositions → 3 deltas in the log.
    for i in 0u8..3 {
        comp.create(test_namespace(), vec![ChunkId([i; 32])], u64::from(i) * 100)
            .unwrap();
    }

    // View starts at watermark 0 in Building state.
    let view = views.get_view(test_view()).unwrap();
    assert_eq!(view.watermark, SequenceNumber(0));
    assert_eq!(view.state, ViewState::Building);

    // Stream processor polls and consumes deltas.
    let mut sp = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp.track(test_view());
    let consumed = sp.poll(1000);

    assert_eq!(consumed, 3);

    // View watermark should be at 3, state Active.
    let view = views.get_view(test_view()).unwrap();
    assert_eq!(view.watermark, SequenceNumber(3));
    assert_eq!(view.state, ViewState::Active);
}

#[test]
fn stream_processor_is_idempotent() {
    let (log, mut comp, mut views) = setup();

    comp.create(test_namespace(), vec![ChunkId([0x01; 32])], 100)
        .unwrap();

    // First poll: consumes 1 delta.
    let mut sp = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp.track(test_view());
    assert_eq!(sp.poll(1000), 1);

    // Second poll: no new deltas.
    let mut sp2 = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp2.track(test_view());
    assert_eq!(sp2.poll(2000), 0);
}

#[test]
fn full_pipeline_write_through_to_view() {
    let (log, mut comp, mut views) = setup();

    // Write data through composition.
    let comp_id = comp
        .create(
            test_namespace(),
            vec![ChunkId([0xAA; 32]), ChunkId([0xBB; 32])],
            2048,
        )
        .unwrap();

    // Verify delta in log.
    let deltas = log
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(1),
        })
        .unwrap();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].header.chunk_refs.len(), 2);

    // Stream processor advances view.
    let mut sp = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp.track(test_view());
    sp.poll(1000);

    // View is now Active at watermark 1.
    let view = views.get_view(test_view()).unwrap();
    assert_eq!(view.state, ViewState::Active);
    assert_eq!(view.watermark, SequenceNumber(1));

    // Composition is still readable.
    let _ = comp.get(comp_id).unwrap();
}
