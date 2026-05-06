//! Persistent audit store — wraps `RaftAuditStore` + `FjallLogStore`.
//!
//! Every audit event is written to both in-memory state machine
//! (fast reads) and the fjall keyspace (durability). On startup,
//! reloads from the keyspace and replays the command log.
//! Per ADR-009/ADR-022 rev-2. Append-only (I-A1).

use std::path::Path;

use kiseki_common::ids::{OrgId, SequenceNumber};
use kiseki_raft::FjallLogStore;

use crate::event::AuditEvent;
use crate::raft_store::{AuditCommand, RaftAuditStore};
use crate::store::{AuditOps, AuditQuery};

/// Persistent audit log — in-memory state machine + fjall for durability.
pub struct PersistentAuditStore {
    inner: RaftAuditStore,
    store: FjallLogStore,
}

impl PersistentAuditStore {
    /// Open or create a persistent audit store at the given path.
    pub fn open(path: &Path) -> Result<Self, String> {
        let store = FjallLogStore::open(path).map_err(|e| format!("fjall open: {e}"))?;

        let entries: Vec<(u64, AuditCommand)> = store.range(1, u64::MAX).unwrap_or_default();

        if entries.is_empty() {
            Ok(Self {
                inner: RaftAuditStore::new(),
                store,
            })
        } else {
            let on_disk_count = entries.len();
            let inner = RaftAuditStore::from_commands(entries.into_iter());
            let log_len = inner.log_length();
            if log_len != on_disk_count {
                tracing::error!(
                    log_len,
                    on_disk_count,
                    "audit store log length differs from persisted entry count"
                );
            }
            Ok(Self { inner, store })
        }
    }
}

impl AuditOps for PersistentAuditStore {
    fn append(&self, event: AuditEvent) {
        // Build the command before appending (for persistence).
        let cmd = AuditCommand::AppendEvent {
            tenant_id: event.tenant_id.map(|o| *o.0.as_bytes()),
            event_type: RaftAuditStore::event_type_to_str_pub(&event.event_type).to_owned(),
            actor: event.actor.clone(),
            description: event.description.clone(),
        };

        // Append to in-memory state machine.
        self.inner.append(event);

        // Persist the command.
        let idx = self.inner.log_length() as u64;
        let _ = self.store.append(idx, &cmd);
    }

    fn query(&self, q: &AuditQuery) -> Vec<AuditEvent> {
        self.inner.query(q)
    }

    fn tip(&self, tenant_id: Option<OrgId>) -> SequenceNumber {
        self.inner.tip(tenant_id)
    }

    fn total_events(&self) -> usize {
        self.inner.total_events()
    }

    fn tenant_export(&self, tenant_id: OrgId) -> Vec<AuditEvent> {
        self.inner.tenant_export(tenant_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::AuditEventType;
    use kiseki_common::ids::NodeId;
    use kiseki_common::time::*;

    fn test_tenant() -> OrgId {
        OrgId(uuid::Uuid::from_u128(100))
    }

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

    fn make_event(tenant: Option<OrgId>, event_type: AuditEventType) -> AuditEvent {
        AuditEvent {
            sequence: SequenceNumber(0),
            timestamp: test_timestamp(),
            event_type,
            tenant_id: tenant,
            actor: "test".into(),
            description: "test event".into(),
        }
    }

    #[test]
    fn append_and_query() {
        let dir = tempfile::tempdir().unwrap();
        let store = PersistentAuditStore::open(&dir.path().join("audit")).unwrap();
        store.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
        assert_eq!(store.total_events(), 1);
        assert_eq!(store.tip(Some(test_tenant())), SequenceNumber(1));
    }

    #[test]
    fn events_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");

        // Write events.
        {
            let store = PersistentAuditStore::open(&path).unwrap();
            store.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
            store.append(make_event(Some(test_tenant()), AuditEventType::KeyRotation));
            store.append(make_event(None, AuditEventType::AdminAction));
            assert_eq!(store.total_events(), 3);
        }

        // Reopen — events should survive.
        {
            let store = PersistentAuditStore::open(&path).unwrap();
            assert_eq!(store.total_events(), 3);
            assert_eq!(store.tip(Some(test_tenant())), SequenceNumber(2));
            assert_eq!(store.tip(None), SequenceNumber(1));

            let events = store.query(&AuditQuery {
                tenant_id: Some(test_tenant()),
                from: SequenceNumber(1),
                limit: 100,
                event_type: None,
            });
            assert_eq!(events.len(), 2);
        }
    }

    #[test]
    fn tenant_export_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit");

        {
            let store = PersistentAuditStore::open(&path).unwrap();
            store.append(make_event(Some(test_tenant()), AuditEventType::DataWrite));
            store.append(make_event(Some(test_tenant()), AuditEventType::DataRead));
        }

        {
            let store = PersistentAuditStore::open(&path).unwrap();
            let export = store.tenant_export(test_tenant());
            assert_eq!(export.len(), 2);
        }
    }
}
