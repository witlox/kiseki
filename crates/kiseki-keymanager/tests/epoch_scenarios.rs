//! Tests for key-management.feature scenarios (manager surface).

use kiseki_common::ids::ChunkId;
use kiseki_common::tenancy::KeyEpoch;
use kiseki_crypto::hkdf::derive_system_dek;
use kiseki_keymanager::epoch::KeyManagerOps;
use kiseki_keymanager::health::KeyManagerStatus;
use kiseki_keymanager::store::MemKeyStore;

// --- Scenario: Initial epoch exists ---
#[test]
fn initial_epoch_created() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let epoch = store.current_epoch().unwrap_or_else(|_| unreachable!());
    assert_eq!(epoch, KeyEpoch(1));

    let epochs = store.list_epochs();
    assert_eq!(epochs.len(), 1);
    assert!(epochs[0].is_current);
    assert!(epochs[0].migration_complete);
}

// --- Scenario: Fetch master key for epoch ---
#[test]
fn fetch_master_key_for_epoch() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let key = store.fetch_master_key(KeyEpoch(1));
    assert!(key.is_ok());

    // Fetching a non-existent epoch fails.
    let missing = store.fetch_master_key(KeyEpoch(99));
    assert!(missing.is_err());
}

// --- Scenario: HKDF derivation is deterministic with fetched key ---
#[test]
fn hkdf_derivation_with_fetched_key() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let master = store
        .fetch_master_key(KeyEpoch(1))
        .unwrap_or_else(|_| unreachable!());
    let chunk_id = ChunkId([0xab; 32]);

    let dek1 = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
    let dek2 = derive_system_dek(&master, &chunk_id).unwrap_or_else(|_| unreachable!());
    assert_eq!(*dek1, *dek2);
}

// --- Scenario: System KEK rotation ---
#[test]
fn key_rotation_creates_new_epoch() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());

    let old_epoch = store.current_epoch().unwrap_or_else(|_| unreachable!());
    assert_eq!(old_epoch, KeyEpoch(1));

    let new_epoch = store.rotate().unwrap_or_else(|_| unreachable!());
    assert_eq!(new_epoch, KeyEpoch(2));

    // New epoch is current.
    assert_eq!(
        store.current_epoch().unwrap_or_else(|_| unreachable!()),
        KeyEpoch(2)
    );

    // Both epochs accessible (I-K6).
    assert!(store.fetch_master_key(KeyEpoch(1)).is_ok());
    assert!(store.fetch_master_key(KeyEpoch(2)).is_ok());

    // Old epoch is not current, migration not yet complete.
    let epochs = store.list_epochs();
    let old = epochs.iter().find(|e| e.epoch == KeyEpoch(1));
    assert!(old.is_some_and(|e| !e.is_current && e.migration_complete));
    let new = epochs.iter().find(|e| e.epoch == KeyEpoch(2));
    assert!(new.is_some_and(|e| e.is_current && !e.migration_complete));
}

// --- Scenario: Different epochs yield different DEKs ---
#[test]
fn different_epochs_different_deks() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.rotate().unwrap_or_else(|_| unreachable!());

    let chunk_id = ChunkId([0xcc; 32]);
    let key1 = store
        .fetch_master_key(KeyEpoch(1))
        .unwrap_or_else(|_| unreachable!());
    let key2 = store
        .fetch_master_key(KeyEpoch(2))
        .unwrap_or_else(|_| unreachable!());

    let dek1 = derive_system_dek(&key1, &chunk_id).unwrap_or_else(|_| unreachable!());
    let dek2 = derive_system_dek(&key2, &chunk_id).unwrap_or_else(|_| unreachable!());

    // Different master keys → different DEKs.
    assert_ne!(*dek1, *dek2);
}

// --- Scenario: Mark migration complete ---
#[test]
fn mark_migration_complete() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.rotate().unwrap_or_else(|_| unreachable!());

    store
        .mark_migration_complete(KeyEpoch(2))
        .unwrap_or_else(|_| unreachable!());

    let epochs = store.list_epochs();
    let e2 = epochs
        .iter()
        .find(|e| e.epoch == KeyEpoch(2))
        .unwrap_or_else(|| unreachable!());
    assert!(e2.migration_complete);
}

// --- Scenario: Multiple rotations ---
#[test]
fn multiple_rotations() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.rotate().unwrap_or_else(|_| unreachable!());
    store.rotate().unwrap_or_else(|_| unreachable!());

    assert_eq!(
        store.current_epoch().unwrap_or_else(|_| unreachable!()),
        KeyEpoch(3)
    );
    assert_eq!(store.list_epochs().len(), 3);

    // All epochs are accessible.
    for i in 1..=3 {
        assert!(store.fetch_master_key(KeyEpoch(i)).is_ok());
    }
}

// --- Scenario: System key manager failure ---
#[test]
fn unavailable_rejects_requests() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    store.set_status(KeyManagerStatus::Unavailable);

    assert!(store.current_epoch().is_err());
    assert!(store.fetch_master_key(KeyEpoch(1)).is_err());
    assert!(store.rotate().is_err());
}

// --- Health reporting ---
#[test]
fn health_reports_correctly() {
    let store = MemKeyStore::new().unwrap_or_else(|_| unreachable!());
    let health = store.health();
    assert_eq!(health.status, KeyManagerStatus::Healthy);
    assert_eq!(health.epoch_count, 1);
    assert_eq!(health.current_epoch, Some(1));

    store.rotate().unwrap_or_else(|_| unreachable!());
    let health = store.health();
    assert_eq!(health.epoch_count, 2);
    assert_eq!(health.current_epoch, Some(2));
}
