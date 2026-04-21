//! Integration test: single-node Raft audit log.
//!
//! Exercises the full path: `Raft::new` -> initialize -> `client_write` ->
//! state machine apply -> read from shared state.

use kiseki_audit::event::{AuditEvent, AuditEventType};
use kiseki_audit::health::AuditStatus;
use kiseki_audit::raft::OpenRaftAuditStore;
use kiseki_audit::store::AuditQuery;
use kiseki_common::ids::{NodeId, OrgId, SequenceNumber};
use kiseki_common::time::*;

fn test_timestamp() -> DeltaTimestamp {
    DeltaTimestamp {
        hlc: HybridLogicalClock {
            physical_ms: 1000,
            logical: 0,
            node_id: NodeId(1),
        },
        wall: WallTime {
            millis_since_epoch: 1000,
            timezone: "UTC".into(),
        },
        quality: ClockQuality::Ntp,
    }
}

fn make_event(
    tenant: Option<OrgId>,
    event_type: AuditEventType,
    actor: &str,
    desc: &str,
) -> AuditEvent {
    AuditEvent {
        sequence: SequenceNumber(0),
        timestamp: test_timestamp(),
        event_type,
        tenant_id: tenant,
        actor: actor.into(),
        description: desc.into(),
    }
}

fn test_tenant() -> OrgId {
    OrgId(uuid::Uuid::from_u128(100))
}

fn test_tenant_2() -> OrgId {
    OrgId(uuid::Uuid::from_u128(200))
}

#[tokio::test]
async fn bootstrap_and_verify() {
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

    // Initial event count should be 0.
    assert_eq!(store.event_count().await, 0);

    // Health should report healthy with zero events.
    let health = store.health().await;
    assert_eq!(health.status, AuditStatus::Healthy);
    assert_eq!(health.event_count, 0);
}

#[tokio::test]
async fn append_through_raft() {
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

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
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

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

#[tokio::test]
async fn event_round_trip() {
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

    // Append via the AuditEvent-based method.
    let event = make_event(
        Some(test_tenant()),
        AuditEventType::DataWrite,
        "user-1",
        "wrote chunk A",
    );
    store.append(event).await.unwrap();

    // Query it back.
    let events = store
        .query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        })
        .await;

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].sequence, SequenceNumber(1));
    assert_eq!(events[0].event_type, AuditEventType::DataWrite);
    assert_eq!(events[0].actor, "user-1");
    assert_eq!(events[0].description, "wrote chunk A");
    assert_eq!(events[0].tenant_id, Some(test_tenant()));
}

#[tokio::test]
async fn per_tenant_isolation() {
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    let t1 = test_tenant();
    let t2 = test_tenant_2();

    store
        .append(make_event(
            Some(t1),
            AuditEventType::DataWrite,
            "user-1",
            "t1 write",
        ))
        .await
        .unwrap();
    store
        .append(make_event(
            Some(t2),
            AuditEventType::DataRead,
            "user-2",
            "t2 read",
        ))
        .await
        .unwrap();
    store
        .append(make_event(
            None,
            AuditEventType::AdminAction,
            "admin",
            "system event",
        ))
        .await
        .unwrap();

    // Each tenant/system shard has exactly one event.
    assert_eq!(store.tip(Some(t1)).await, SequenceNumber(1));
    assert_eq!(store.tip(Some(t2)).await, SequenceNumber(1));
    assert_eq!(store.tip(None).await, SequenceNumber(1));
    assert_eq!(store.total_events().await, 3);

    // Query t1 — should only see t1's event.
    let t1_events = store
        .query(&AuditQuery {
            tenant_id: Some(t1),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        })
        .await;
    assert_eq!(t1_events.len(), 1);
    assert_eq!(t1_events[0].actor, "user-1");

    // Query t2 — should only see t2's event.
    let t2_events = store
        .query(&AuditQuery {
            tenant_id: Some(t2),
            from: SequenceNumber(1),
            limit: 100,
            event_type: None,
        })
        .await;
    assert_eq!(t2_events.len(), 1);
    assert_eq!(t2_events[0].actor, "user-2");
}

#[tokio::test]
async fn tenant_export_returns_only_tenant_events() {
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();
    let t1 = test_tenant();
    let t2 = test_tenant_2();

    store
        .append(make_event(
            Some(t1),
            AuditEventType::DataWrite,
            "user-1",
            "t1 write 1",
        ))
        .await
        .unwrap();
    store
        .append(make_event(
            Some(t2),
            AuditEventType::DataRead,
            "user-2",
            "t2 read",
        ))
        .await
        .unwrap();
    store
        .append(make_event(
            Some(t1),
            AuditEventType::KeyRotation,
            "admin",
            "t1 key rot",
        ))
        .await
        .unwrap();

    let export = store.tenant_export(t1).await;
    assert_eq!(export.len(), 2);
    assert_eq!(export[0].description, "t1 write 1");
    assert_eq!(export[1].description, "t1 key rot");

    // t2 export should have only 1 event.
    let export_t2 = store.tenant_export(t2).await;
    assert_eq!(export_t2.len(), 1);
    assert_eq!(export_t2[0].description, "t2 read");
}

#[tokio::test]
async fn event_metadata_preserved() {
    let store = OpenRaftAuditStore::new(1, &std::collections::BTreeMap::new())
        .await
        .unwrap();

    store
        .append(make_event(
            Some(test_tenant()),
            AuditEventType::KeyRotation,
            "security-bot",
            "rotated tenant KEK",
        ))
        .await
        .unwrap();

    let events = store
        .query(&AuditQuery {
            tenant_id: Some(test_tenant()),
            from: SequenceNumber(1),
            limit: 10,
            event_type: Some(AuditEventType::KeyRotation),
        })
        .await;

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, AuditEventType::KeyRotation);
    assert_eq!(events[0].actor, "security-bot");
    assert_eq!(events[0].description, "rotated tenant KEK");
}
