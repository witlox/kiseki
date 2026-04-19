//! Integration test: single-node Raft log store.
//!
//! Exercises the full path: `Raft::new` -> initialize -> `client_write` ->
//! state machine apply -> read from shared state.

use kiseki_common::ids::SequenceNumber;
use kiseki_log::raft::OpenRaftLogStore;

#[tokio::test]
async fn bootstrap_and_verify() {
    let store = OpenRaftLogStore::new(1).await.unwrap();

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
}

#[tokio::test]
async fn append_through_raft() {
    let store = OpenRaftLogStore::new(1).await.unwrap();

    let seq = store
        .append_delta(
            [0u8; 16],      // tenant_id_bytes
            0,              // operation (Create)
            [0x50; 32],     // hashed_key
            vec![],         // chunk_refs
            vec![0xab; 64], // payload
            false,          // has_inline_data
        )
        .await
        .unwrap();

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
async fn maintenance_through_raft() {
    let store = OpenRaftLogStore::new(1).await.unwrap();

    // Enable maintenance.
    store.set_maintenance(true).await.unwrap();
    assert!(store.is_maintenance().await);

    let health = store.shard_health().await;
    assert_eq!(health.state, kiseki_log::ShardState::Maintenance);

    // Disable maintenance.
    store.set_maintenance(false).await.unwrap();
    assert!(!store.is_maintenance().await);

    let health = store.shard_health().await;
    assert_eq!(health.state, kiseki_log::ShardState::Healthy);
}

#[tokio::test]
async fn multiple_appends() {
    let store = OpenRaftLogStore::new(1).await.unwrap();

    for i in 0u8..5 {
        let seq = store
            .append_delta(
                [0u8; 16],
                0,
                [i * 20 + 10; 32],
                vec![],
                vec![0xab; 64],
                false,
            )
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
