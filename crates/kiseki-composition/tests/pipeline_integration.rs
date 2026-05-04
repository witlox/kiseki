#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end pipeline test: Composition store operations + log/view plumbing.
//!
//! Composition mutations are now sync (in-memory only). Log emission is the
//! gateway's responsibility. These tests verify the composition store CRUD
//! works correctly and that the log/view infrastructure functions independently.

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

/// Set up the full pipeline: log store + composition store + view store.
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
        versioning_enabled: false,
        compliance_tags: Vec::new(),
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
fn create_composition_stores_in_memory() {
    let (_log, mut comp, _views) = setup();

    // Create a composition — sync, no log emission.
    let comp_id = comp
        .create(test_namespace(), vec![ChunkId([0x01; 32])], 1024)
        .unwrap();

    // Composition is immediately readable.
    let c = comp.get(comp_id).unwrap();
    assert_eq!(c.tenant_id, test_tenant());
    assert_eq!(c.size, 1024);
    assert_eq!(c.chunks.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn composition_does_not_emit_deltas_to_log() {
    // Composition is now sync — it does NOT write to the log.
    // Log emission is the gateway's responsibility.
    let (log, mut comp, _views) = setup();

    let _comp_id = comp
        .create(test_namespace(), vec![ChunkId([0x01; 32])], 1024)
        .unwrap();

    let deltas = log
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(100),
        })
        .await
        .unwrap();

    // Gateway hasn't emitted anything — log is empty.
    assert_eq!(deltas.len(), 0);
}

#[test]
fn update_and_delete_operate_on_in_memory_store() {
    let (_log, mut comp, _views) = setup();

    let comp_id = comp
        .create(test_namespace(), vec![ChunkId([0x01; 32])], 100)
        .unwrap();

    let v2 = comp
        .update(comp_id, vec![ChunkId([0x02; 32])], 200)
        .unwrap();
    assert_eq!(v2, 2);

    let del = comp.delete(comp_id).unwrap();
    assert!(matches!(del, kiseki_composition::DeleteResult::Removed(_)));
    assert!(comp.get(comp_id).is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_processor_does_not_advance_without_gateway_deltas() {
    // Without the gateway emitting deltas, the log stays empty and the
    // stream processor has nothing to consume.
    let (log, mut comp, mut views) = setup();

    for i in 0u8..3 {
        comp.create(test_namespace(), vec![ChunkId([i; 32])], u64::from(i) * 100)
            .unwrap();
    }

    // View starts at watermark 0 in Building state.
    let view = views.get_view(test_view()).unwrap();
    assert_eq!(view.watermark, SequenceNumber(0));
    assert_eq!(view.state, ViewState::Building);

    // Stream processor polls — log is empty (composition didn't emit).
    let mut sp = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp.track(test_view());
    let consumed = sp.poll(1000).await;

    assert_eq!(consumed, 0);

    // Watermark stays at 0, view stays Building.
    let view = views.get_view(test_view()).unwrap();
    assert_eq!(view.watermark, SequenceNumber(0));
    assert_eq!(view.state, ViewState::Building);
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_processor_is_idempotent_on_empty_log() {
    let (log, mut comp, mut views) = setup();

    comp.create(test_namespace(), vec![ChunkId([0x01; 32])], 100)
        .unwrap();

    // Both polls return 0 — composition didn't write to the log.
    let mut sp = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp.track(test_view());
    assert_eq!(sp.poll(1000).await, 0);

    let mut sp2 = TrackedStreamProcessor::new(log.as_ref(), &mut views);
    sp2.track(test_view());
    assert_eq!(sp2.poll(2000).await, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn composition_store_crud_independent_of_log() {
    let (log, mut comp, _views) = setup();

    // Write through composition store.
    let comp_id = comp
        .create(
            test_namespace(),
            vec![ChunkId([0xAA; 32]), ChunkId([0xBB; 32])],
            2048,
        )
        .unwrap();

    // Log has no deltas — gateway hasn't emitted anything.
    let deltas = log
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(1),
        })
        .await
        .unwrap();
    assert_eq!(deltas.len(), 0);

    // Composition is still readable from the in-memory store.
    let c = comp.get(comp_id).unwrap();
    assert_eq!(c.chunks.len(), 2);
    assert_eq!(c.size, 2048);
}
