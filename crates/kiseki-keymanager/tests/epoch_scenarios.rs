//! Tests for key-management.feature scenarios (manager surface).

use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::hkdf::derive_system_dek;
use kiseki_keymanager::epoch::KeyManagerOps;
use kiseki_keymanager::health::KeyManagerStatus;
use kiseki_keymanager::store::MemKeyStore;

#[tokio::test]
async fn initial_epoch_created() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let epoch = store
        .current_epoch()
        .await
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(epoch, KeyEpoch(1));

    let epochs = store.list_epochs().await;
    assert_eq!(epochs.len(), 1);
    assert!(epochs[0].is_current);
    assert!(epochs[0].migration_complete);
}

#[tokio::test]
async fn fetch_master_key_for_epoch() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let key = store.fetch_master_key(KeyEpoch(1)).await;
    assert!(key.is_ok());

    let missing = store.fetch_master_key(KeyEpoch(99)).await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn hkdf_derivation_with_fetched_key() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let master = store
        .fetch_master_key(KeyEpoch(1))
        .await
        .unwrap_or_else(|_| unreachable!());
    let chunk_id = ChunkId([0xab; 32]);

    let dek1 = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
    let dek2 = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
    assert_eq!(*dek1, *dek2);
}

#[tokio::test]
async fn key_rotation_creates_new_epoch() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());

    let old_epoch = store
        .current_epoch()
        .await
        .unwrap_or_else(|_| unreachable!());
    assert_eq!(old_epoch, KeyEpoch(1));

    let new_epoch = store.rotate().await.unwrap_or_else(|_| unreachable!());
    assert_eq!(new_epoch, KeyEpoch(2));

    assert_eq!(
        store
            .current_epoch()
            .await
            .unwrap_or_else(|_| unreachable!()),
        KeyEpoch(2)
    );

    assert!(store.fetch_master_key(KeyEpoch(1)).await.is_ok());
    assert!(store.fetch_master_key(KeyEpoch(2)).await.is_ok());

    let epochs = store.list_epochs().await;
    let old = epochs.iter().find(|e| e.epoch == KeyEpoch(1));
    assert!(old.is_some_and(|e| !e.is_current && e.migration_complete));
    let new = epochs.iter().find(|e| e.epoch == KeyEpoch(2));
    assert!(new.is_some_and(|e| e.is_current && !e.migration_complete));
}

#[tokio::test]
async fn different_epochs_different_deks() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.rotate().await.unwrap_or_else(|_| unreachable!());

    let chunk_id = ChunkId([0xcc; 32]);
    let key1 = store
        .fetch_master_key(KeyEpoch(1))
        .await
        .unwrap_or_else(|_| unreachable!());
    let key2 = store
        .fetch_master_key(KeyEpoch(2))
        .await
        .unwrap_or_else(|_| unreachable!());

    let dek1 = derive_system_dek(&key1, &chunk_id).unwrap_or_else(|_| unreachable!());
    let dek2 = derive_system_dek(&key2, &chunk_id).unwrap_or_else(|_| unreachable!());
    assert_ne!(*dek1, *dek2);
}

#[tokio::test]
async fn mark_migration_complete() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.rotate().await.unwrap_or_else(|_| unreachable!());

    store
        .mark_migration_complete(KeyEpoch(2))
        .await
        .unwrap_or_else(|_| unreachable!());

    let epochs = store.list_epochs().await;
    let e2 = epochs
        .iter()
        .find(|e| e.epoch == KeyEpoch(2))
        .unwrap_or_else(|| unreachable!());
    assert!(e2.migration_complete);
}

#[tokio::test]
async fn multiple_rotations() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.rotate().await.unwrap_or_else(|_| unreachable!());
    store.rotate().await.unwrap_or_else(|_| unreachable!());

    assert_eq!(
        store
            .current_epoch()
            .await
            .unwrap_or_else(|_| unreachable!()),
        KeyEpoch(3)
    );
    assert_eq!(store.list_epochs().await.len(), 3);

    for i in 1..=3 {
        assert!(store.fetch_master_key(KeyEpoch(i)).await.is_ok());
    }
}

#[tokio::test]
async fn unavailable_rejects_requests() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.set_status(KeyManagerStatus::Unavailable);

    assert!(store.current_epoch().await.is_err());
    assert!(store.fetch_master_key(KeyEpoch(1)).await.is_err());
    assert!(store.rotate().await.is_err());
}

#[tokio::test]
async fn health_reports_correctly() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let health = store.health();
    assert_eq!(health.status, KeyManagerStatus::Healthy);
    assert_eq!(health.epoch_count, 1);
    assert_eq!(health.current_epoch, Some(1));

    store.rotate().await.unwrap_or_else(|_| unreachable!());
    let health = store.health();
    assert_eq!(health.epoch_count, 2);
    assert_eq!(health.current_epoch, Some(2));
}
