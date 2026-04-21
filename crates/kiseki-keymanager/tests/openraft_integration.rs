//! Integration test: single-node Raft key manager.
//!
//! Exercises the full path: `Raft::new` → initialize → `client_write` →
//! state machine apply → read from shared state.

use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::hkdf::derive_system_dek;
use kiseki_keymanager::epoch::KeyManagerOps;
use kiseki_keymanager::raft::OpenRaftKeyStore;

#[tokio::test]
async fn bootstrap_and_read_epoch() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

    // Initial epoch should be 1.
    let epoch = store.current_epoch().await.unwrap();
    assert_eq!(epoch, KeyEpoch(1));

    // Fetch master key for epoch 1.
    let key = store.fetch_master_key(KeyEpoch(1)).await;
    assert!(key.is_ok());

    // Non-existent epoch fails.
    let missing = store.fetch_master_key(KeyEpoch(99)).await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn rotate_through_raft() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

    let new_epoch = store.rotate().await.unwrap();
    assert_eq!(new_epoch, KeyEpoch(2));

    // Both epochs accessible.
    assert!(store.fetch_master_key(KeyEpoch(1)).await.is_ok());
    assert!(store.fetch_master_key(KeyEpoch(2)).await.is_ok());

    // Current is epoch 2.
    assert_eq!(store.current_epoch().await.unwrap(), KeyEpoch(2));

    // List shows both.
    let epochs = store.list_epochs().await;
    assert_eq!(epochs.len(), 2);
}

#[tokio::test]
async fn hkdf_works_through_raft() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    let master = store.fetch_master_key(KeyEpoch(1)).await.unwrap();
    let chunk_id = ChunkId([0xab; 32]);

    let dek1 = derive_system_dek(&master, &chunk_id).unwrap();
    let dek2 = derive_system_dek(&master, &chunk_id).unwrap();
    assert_eq!(*dek1, *dek2);
}

#[tokio::test]
async fn different_epochs_different_keys() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    store.rotate().await.unwrap();

    let key1 = store.fetch_master_key(KeyEpoch(1)).await.unwrap();
    let key2 = store.fetch_master_key(KeyEpoch(2)).await.unwrap();
    let chunk_id = ChunkId([0xcc; 32]);

    let dek1 = derive_system_dek(&key1, &chunk_id).unwrap();
    let dek2 = derive_system_dek(&key2, &chunk_id).unwrap();
    assert_ne!(*dek1, *dek2);
}

#[tokio::test]
async fn mark_migration_complete_through_raft() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    store.rotate().await.unwrap();

    store.mark_migration_complete(KeyEpoch(2)).await.unwrap();

    let epochs = store.list_epochs().await;
    let e2 = epochs.iter().find(|e| e.epoch == KeyEpoch(2)).unwrap();
    assert!(e2.migration_complete);
}

#[tokio::test]
async fn multiple_rotations_through_raft() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    store.rotate().await.unwrap();
    store.rotate().await.unwrap();

    assert_eq!(store.current_epoch().await.unwrap(), KeyEpoch(3));
    assert_eq!(store.list_epochs().await.len(), 3);

    for i in 1..=3 {
        assert!(store.fetch_master_key(KeyEpoch(i)).await.is_ok());
    }
}

#[tokio::test]
async fn health_reports_correctly() {
    let store = OpenRaftKeyStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    let health = store.health().await;
    assert_eq!(health.epoch_count, 1);
    assert_eq!(health.current_epoch, Some(1));

    store.rotate().await.unwrap();
    let health = store.health().await;
    assert_eq!(health.epoch_count, 2);
    assert_eq!(health.current_epoch, Some(2));
}
