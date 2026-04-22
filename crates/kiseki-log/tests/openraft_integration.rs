//! Integration test: single-node Raft log store.
//!
//! Exercises the full path: `Raft::new` -> initialize -> `client_write` ->
//! state machine apply -> read from shared state.

use kiseki_common::ids::{OrgId, SequenceNumber, ShardId};
use kiseki_log::raft::OpenRaftLogStore;
use kiseki_log::traits::{AppendDeltaRequest, ReadDeltasRequest};
use kiseki_log::OperationType;

fn test_shard() -> ShardId {
    ShardId(uuid::Uuid::from_u128(1))
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn make_append_req(key_byte: u8) -> AppendDeltaRequest {
    AppendDeltaRequest {
        shard_id: test_shard(),
        tenant_id: test_tenant(),
        operation: OperationType::Create,
        timestamp: kiseki_common::time::DeltaTimestamp {
            hlc: kiseki_common::time::HybridLogicalClock {
                physical_ms: 1000,
                logical: 0,
                node_id: kiseki_common::ids::NodeId(1),
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

#[tokio::test]
async fn bootstrap_and_verify() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    // Initial tip should be 0 (no deltas appended).
    let tip = store.current_tip().await;
    assert_eq!(tip, SequenceNumber(0));

    // Should not be in maintenance mode.
    assert!(!store.is_maintenance().await);

    // Health should report healthy with zero deltas.
    let health = store.shard_health().await;
    assert_eq!(health.delta_count, 0);
    assert_eq!(health.tip, SequenceNumber(0));
    assert_eq!(health.state, kiseki_log::ShardState::Healthy);
    assert_eq!(health.shard_id, test_shard());
    assert_eq!(health.tenant_id, test_tenant());
}

#[tokio::test]
async fn append_through_raft() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    let seq = store.append_delta(make_append_req(0x50)).await.unwrap();
    assert_eq!(seq, SequenceNumber(1));

    // Tip should advance to 1.
    let tip = store.current_tip().await;
    assert_eq!(tip, SequenceNumber(1));

    // Health should reflect the append.
    let health = store.shard_health().await;
    assert_eq!(health.delta_count, 1);
    assert_eq!(health.tip, SequenceNumber(1));
}

#[tokio::test]
async fn append_and_read_deltas_round_trip() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    // Append 3 deltas with different keys.
    for i in 0u8..3 {
        store
            .append_delta(make_append_req(i * 10 + 5))
            .await
            .unwrap();
    }

    // Read all 3 back.
    let deltas = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(3),
        })
        .await
        .unwrap();

    assert_eq!(deltas.len(), 3);
    for (i, d) in deltas.iter().enumerate() {
        assert_eq!(d.header.sequence, SequenceNumber(i as u64 + 1));
        assert_eq!(d.header.shard_id, test_shard());
        assert_eq!(d.header.operation, OperationType::Create);
        assert_eq!(d.payload.ciphertext, vec![0xab; 64]);
    }

    // Read a subset.
    let subset = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(2),
            to: SequenceNumber(2),
        })
        .await
        .unwrap();
    assert_eq!(subset.len(), 1);
    assert_eq!(subset[0].header.sequence, SequenceNumber(2));
}

#[tokio::test]
async fn maintenance_through_raft() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    // Enable maintenance.
    store.set_maintenance(true).await.unwrap();
    assert!(store.is_maintenance().await);

    let health = store.shard_health().await;
    assert_eq!(health.state, kiseki_log::ShardState::Maintenance);

    // Append should be rejected in maintenance mode.
    let result = store.append_delta(make_append_req(0x50)).await;
    assert!(result.is_err());

    // Disable maintenance.
    store.set_maintenance(false).await.unwrap();
    assert!(!store.is_maintenance().await);

    let health = store.shard_health().await;
    assert_eq!(health.state, kiseki_log::ShardState::Healthy);
}

#[tokio::test]
async fn multiple_appends() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    for i in 0u8..5 {
        let seq = store
            .append_delta(make_append_req(i * 20 + 10))
            .await
            .unwrap();
        assert_eq!(seq, SequenceNumber(u64::from(i) + 1));
    }

    // Tip should be 5 after 5 appends.
    let tip = store.current_tip().await;
    assert_eq!(tip, SequenceNumber(5));

    // Health should reflect 5 deltas.
    let health = store.shard_health().await;
    assert_eq!(health.delta_count, 5);
}

#[tokio::test]
async fn watermark_advancement() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    // Append some deltas.
    for i in 0u8..5 {
        store.append_delta(make_append_req(i * 10)).await.unwrap();
    }

    // Register and advance watermarks.
    store
        .register_consumer("view-nfs", SequenceNumber(1))
        .await
        .unwrap();
    store
        .advance_watermark("view-nfs", SequenceNumber(3))
        .await
        .unwrap();
    store
        .register_consumer("audit", SequenceNumber(2))
        .await
        .unwrap();

    // Truncate should use minimum watermark (audit at 2).
    let gc_boundary = store.truncate_log().await.unwrap();
    assert_eq!(gc_boundary, SequenceNumber(2));

    // Deltas below 2 should be gone; 2..=5 should remain.
    let remaining = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(5),
        })
        .await
        .unwrap();
    assert_eq!(remaining.len(), 4); // seq 2, 3, 4, 5
    assert_eq!(remaining[0].header.sequence, SequenceNumber(2));
}

#[tokio::test]
async fn compact_shard_deduplicates() {
    let store = OpenRaftLogStore::new(
        1,
        test_shard(),
        test_tenant(),
        &std::collections::BTreeMap::new(),
        None,
        None,
    )
    .await
    .unwrap();

    // Append two deltas with the same key — second supersedes first.
    store.append_delta(make_append_req(0x42)).await.unwrap();
    store.append_delta(make_append_req(0x42)).await.unwrap();
    // And one with a different key.
    store.append_delta(make_append_req(0x99)).await.unwrap();

    assert_eq!(store.current_tip().await, SequenceNumber(3));

    let removed = store.compact_shard().await.unwrap();
    assert_eq!(removed, 1); // the first 0x42 delta was removed

    // Should have 2 deltas left (latest 0x42 and 0x99).
    let remaining = store
        .read_deltas(ReadDeltasRequest {
            shard_id: test_shard(),
            from: SequenceNumber(1),
            to: SequenceNumber(3),
        })
        .await
        .unwrap();
    assert_eq!(remaining.len(), 2);
}
