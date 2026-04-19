//! Integration test: single-node Raft audit log.
//!
//! Exercises the full path: `Raft::new` -> initialize -> `client_write` ->
//! state machine apply -> read from shared state.

use kiseki_audit::health::AuditStatus;
use kiseki_audit::raft::OpenRaftAuditStore;

#[tokio::test]
async fn bootstrap_and_verify() {
    let store = OpenRaftAuditStore::new(1).await.unwrap();

    // Initial event count should be 0.
    assert_eq!(store.event_count().await, 0);

    // Health should report healthy with zero events.
    let health = store.health().await;
    assert_eq!(health.status, AuditStatus::Healthy);
    assert_eq!(health.event_count, 0);
}

#[tokio::test]
async fn append_through_raft() {
    let store = OpenRaftAuditStore::new(1).await.unwrap();

    store
        .append_event("DataWrite", "user-1", None, "wrote a chunk")
        .await
        .unwrap();

    assert_eq!(store.event_count().await, 1);

    let health = store.health().await;
    assert_eq!(health.event_count, 1);
}

#[tokio::test]
async fn multiple_appends() {
    let store = OpenRaftAuditStore::new(1).await.unwrap();

    store
        .append_event("DataWrite", "user-1", None, "wrote chunk 1")
        .await
        .unwrap();
    store
        .append_event("DataRead", "user-2", Some([0xab; 16]), "read chunk 2")
        .await
        .unwrap();
    store
        .append_event("KeyRotation", "admin", None, "rotated keys")
        .await
        .unwrap();

    assert_eq!(store.event_count().await, 3);

    let health = store.health().await;
    assert_eq!(health.status, AuditStatus::Healthy);
    assert_eq!(health.event_count, 3);
}
